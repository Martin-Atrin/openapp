// Generate OpenApp's third-party notices for a public macOS release. No dependencies besides node, cargo, pnpm, tar.
// Usage: node scripts/licenses.ts [--out-dir DIR] [--guest-tar FILE] [--skip-guest] [--online]
//   --out-dir    where THIRD-PARTY-NOTICES.md and guest-image-packages.txt are written (default: repo root)
//   --guest-tar  guest image tar (default: <artifacts>/openapp/openapp-guest-<ver>.tar, ver from the Dockerfile label)
//   --skip-guest do not read the guest image (the runtime-download section lists packages as not generated)
//   --online     re-check the pinned runtime downloads (Pi, Codex) with `npm view` and warn on drift
// Inputs (all read-only, nothing is booted or installed):
//   (a) Rust crates linked into the release binary: `cargo metadata` + `cargo tree -e normal,no-proc-macro
//       --target aarch64-apple-darwin` (normal edges only; proc-macro crates and their deps are listed separately
//       as build-time only), licence files from the cargo registry source dirs.
//   (b) npm production deps of the shell bundle: `pnpm licenses list --prod --json` (type-only packages are listed apart).
//   (c) bundled binaries: msb 0.7.8, libkrunfw.5.dylib, the Swift sidecar (facts are constants below, hashes are
//       checked against the bundle inputs named in src-tauri/tauri.conf.json).
//   (d) runtime downloads (not shipped): guest image (dpkg status + /usr/share/doc/*/copyright read straight out
//       of the image tar, nothing booted), Node, Pi, Codex.
// Review flags (GPL/AGPL/SSPL/unknown in the binary, missing licence fields, UNLICENSED, ...) go to stdout only.
// Licence texts are deduplicated by the hash of the text without its copyright lines; holders stay on each entry.

import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, lstatSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const tauriDir = join(root, "src-tauri");
const TARGET = "aarch64-apple-darwin";

// ---- command line -----------------------------------------------------------------------------------------------

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
const has = (name: string): boolean => process.argv.includes(name);
for (const a of process.argv.slice(2)) {
  if (a.startsWith("--") && !["--out-dir", "--guest-tar", "--skip-guest", "--online"].includes(a)) {
    console.error(`unknown option ${a}\nusage: node scripts/licenses.ts [--out-dir DIR] [--guest-tar FILE] [--skip-guest] [--online]`);
    process.exit(2);
  }
}
const outDir = resolve(arg("--out-dir") ?? root);

const flags: string[] = []; // review flags, printed at the end
const flag = (msg: string): void => {
  flags.push(msg);
};

// ---- helpers ----------------------------------------------------------------------------------------------------

const sha256 = (data: string | Buffer): string => createHash("sha256").update(data).digest("hex");

function run(cmd: string, args: string[], cwd: string): string {
  const r = spawnSync(cmd, args, { cwd, encoding: "utf8", maxBuffer: 512 * 1024 * 1024 });
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed (${r.status}): ${r.stderr.slice(0, 600)}`);
  return r.stdout;
}

function readText(path: string): string | null {
  try {
    if (!lstatSync(path).isFile()) return null;
    const buf = readFileSync(path);
    if (buf.length > 400_000) return null; // not a licence text
    return buf.toString("utf8").replace(/^﻿/, "").replace(/\r\n?/g, "\n").trimEnd();
  } catch {
    return null;
  }
}

const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicen[cs]e|copyright|patents)([-_. ].*)?$/i;

/** Licence-like files at the top of a package dir (plus one level of licenses/ dirs). */
function licenseFiles(dir: string, declared?: string | null): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  const add = (p: string): void => {
    if (!seen.has(p) && readText(p) !== null) {
      seen.add(p);
      out.push(p);
    }
  };
  if (declared) add(join(dir, declared));
  let names: string[] = [];
  try {
    names = readdirSync(dir).sort();
  } catch {
    return out;
  }
  for (const n of names) {
    const p = join(dir, n);
    if (LICENSE_FILE.test(n) && !/\.(spdx|license)$/i.test(n)) {
      add(p);
    } else if (/^licen[cs]es?$/i.test(n)) {
      try {
        for (const m of readdirSync(p).sort()) add(join(p, m));
      } catch {
        /* not a dir */
      }
    }
  }
  return out;
}

// ---- licence classification -------------------------------------------------------------------------------------
// 0 permissive, 1 weak copyleft (file/library level), 2 strong copyleft or network copyleft, 3 unknown/unlicensed.

const CLASS_NAME = ["permissive", "weak copyleft", "strong copyleft", "unknown or missing"] as const;

function classifyId(id: string): number {
  if (/^(MIT|MIT-0|Apache-2\.0|Apache-1\.1|BSD-[0-9A-Za-z-]+|0BSD|ISC|Zlib|Unlicense|CC0-1\.0|BSL-1\.0|Unicode-[0-9A-Z.-]+|OpenSSL|BlueOak-1\.0\.0|CDLA-Permissive-[0-9.]+|NCSA|WTFPL|Python-2\.0|X11|curl|HPND|Libpng|MS-PL|Beerware)$/.test(id)) return 0;
  if (/^(MPL-[0-9.]+|LGPL-[0-9.]+(-only|-or-later|\+)?|EPL-[0-9.]+|CDDL-[0-9.]+)$/.test(id)) return 1;
  if (/^(GPL-[0-9.]+(-only|-or-later|\+)?|AGPL-[0-9.]+(-only|-or-later|\+)?|SSPL-[0-9.]+|EUPL-[0-9.]+|OSL-[0-9.]+|CC-BY-SA-[0-9.]+)$/.test(id)) return 2;
  return 3;
}

/** SPDX expression -> highest obligation class we cannot avoid (OR picks the easier side). */
function classifyExpr(expr: string | null): number {
  if (!expr) return 3;
  const toks = expr.replace(/\//g, " OR ").match(/\(|\)|[A-Za-z0-9.+_-]+/g) ?? [];
  let i = 0;
  const peek = (): string | undefined => toks[i];
  const orExpr = (): number => {
    let v = andExpr();
    while (peek()?.toUpperCase() === "OR") {
      i++;
      v = Math.min(v, andExpr());
    }
    return v;
  };
  const andExpr = (): number => {
    let v = withExpr();
    while (peek()?.toUpperCase() === "AND") {
      i++;
      v = Math.max(v, withExpr());
    }
    return v;
  };
  const withExpr = (): number => {
    const v = atom();
    if (peek()?.toUpperCase() === "WITH") {
      i += 2; // exception id: does not change the class
    }
    return v;
  };
  const atom = (): number => {
    const t = toks[i++];
    if (t === undefined) return 3;
    if (t === "(") {
      const v = orExpr();
      if (peek() === ")") i++;
      return v;
    }
    return classifyId(t);
  };
  const v = orExpr();
  return i < toks.length ? 3 : v; // unparsed tail: unknown
}

const exprIds = (expr: string): string[] =>
  [...new Set((expr.replace(/\//g, " OR ").match(/[A-Za-z0-9.+_-]+/g) ?? []).filter((t) => !/^(AND|OR|WITH)$/i.test(t)))];

// ---- licence text registry --------------------------------------------------------------------------------------

type TextEntry = { key: string; title: string; variants: Map<string, string>; users: Set<string>; firstUser: string };
const texts = new Map<string, TextEntry>();

const YEAR = /\b(19|20)\d{2}\b/;
function isHolderLine(line: string): boolean {
  const t = line.trim().replace(/^[*#>/\-\s]+/, "");
  if (!/^(copyright|\(c\)|©)/i.test(t)) return false;
  if (/\[yyyy]|<year>|\{yyyy}|\{year}|\[year]|\byear\b.*\bname\b|name of (the )?(copyright )?(owner|author)|copyright (notice|owner|holder|and related|law|license|statement)/i.test(t)) return false;
  if (/^\(c\)\s+[A-Za-z]\)/i.test(t) || /^\(c\)\s+(you|if|the|and|in|any)\b/i.test(t)) return false; // list item, not a holder
  return YEAR.test(t) || /contributors|authors|developers/i.test(t) || /^copyright\s+(\(c\)\s+)?[A-Z]/.test(t);
}

function titleOf(body: string): string {
  const b = body.slice(0, 6000).replace(/\s+/g, " ");
  const head = b.slice(0, 400);
  if (/Mozilla Public License/i.test(head)) return "MPL-2.0";
  if (/GNU LESSER GENERAL PUBLIC LICENSE/i.test(head) && /Version 2\.1/.test(head)) return "LGPL-2.1";
  if (/GNU GENERAL PUBLIC LICENSE/i.test(head) && /Version 2,/.test(head)) return "GPL-2.0";
  if (/GNU GENERAL PUBLIC LICENSE/i.test(head) && /Version 3,/.test(head)) return "GPL-3.0";
  if (/Apache License/i.test(b) && /Version 2\.0/.test(b)) return "Apache-2.0";
  if (/Boost Software License/i.test(b)) return "BSL-1.0";
  if (/Permission is hereby granted, free of charge/i.test(b)) return /Software is furnished/i.test(b) ? "MIT" : "MIT-like";
  if (/Redistribution and use in source and binary forms/i.test(b)) return /Neither the name|may be used to endorse/i.test(b) ? "BSD-3-Clause" : "BSD-2-Clause";
  if (/Permission to use, copy, modify, and\/or distribute/i.test(b)) return "ISC";
  if (/zlib/i.test(b) && /altered source versions/i.test(b)) return "Zlib";
  if (/UNICODE LICENSE|Unicode, Inc\./i.test(b)) return "Unicode";
  if (/This is free and unencumbered software released into the public domain/i.test(b)) return "Unlicense";
  if (/Creative Commons|CC0/i.test(b)) return "CC0-1.0";
  if (/OpenSSL License|SSLeay/i.test(b)) return "OpenSSL";
  if (/Blue Oak Model License/i.test(b)) return "BlueOak-1.0.0";
  if (/Community Data License Agreement/i.test(b)) return "CDLA-Permissive-2.0";
  if (/^\s*NOTICE\b|This product includes software/i.test(b)) return "NOTICE";
  return "Other";
}

/** Register one licence text for a component; returns its dedupe key. */
function addText(raw: string, user: string): string {
  const body = raw.trim();
  // The key ignores what varies per component: holder lines, the Apache appendix, headings above an MIT permission notice.
  let core = body.split(/END OF TERMS AND CONDITIONS/i)[0];
  const perm = core.search(/Permission is hereby granted, free of charge/i);
  if (perm > 0) core = core.slice(perm);
  const norm = core
    .split("\n")
    .filter((l) => !isHolderLine(l))
    .join("\n")
    .replace(/\s+/g, " ")
    .trim();
  const key = sha256(norm).slice(0, 12);
  let e = texts.get(key);
  if (!e) {
    e = { key, title: titleOf(body), variants: new Map(), users: new Set(), firstUser: user };
    texts.set(key, e);
  }
  e.variants.set(sha256(body), body);
  e.users.add(user);
  return key;
}

/** Holder lines found in a component's licence files (first two, trimmed). */
function holders(bodies: string[]): string[] {
  const out: string[] = [];
  for (const b of bodies) {
    for (const l of b.split("\n")) {
      if (!isHolderLine(l)) continue;
      const t = l.trim().replace(/^[*#>/\-\s]+/, "").replace(/\s+/g, " ");
      const c = t.length > 110 ? `${t.slice(0, 107)}...` : t;
      if (!out.includes(c)) out.push(c);
      if (out.length >= 2) return out;
    }
  }
  return out;
}

// ---- component model --------------------------------------------------------------------------------------------

type Component = {
  name: string;
  version: string;
  license: string | null;
  url: string;
  cls: number;
  copyrights: string[];
  textKeys: string[];
  standardText: boolean; // package ships no licence file; the standard text from another component is referenced
  noFile: boolean;
};

/** Attach licence files of a package dir to a component. */
function attachFiles(c: Component, dir: string, declared: string | null, user: string): void {
  const files = licenseFiles(dir, declared);
  const bodies: string[] = [];
  for (const f of files) {
    const t = readText(f);
    if (t === null || t.trim() === "") continue;
    bodies.push(t);
    const k = addText(t, user);
    if (!c.textKeys.includes(k)) c.textKeys.push(k);
  }
  c.copyrights = holders(bodies);
  c.noFile = c.textKeys.length === 0;
}

// ---- (a) Rust crates --------------------------------------------------------------------------------------------

type CargoPkg = {
  name: string;
  version: string;
  id: string;
  source: string | null;
  license: string | null;
  license_file: string | null;
  repository: string | null;
  homepage: string | null;
  manifest_path: string;
  authors: string[];
  targets: { kind: string[] }[];
};

function treeSet(edges: string): Set<string> {
  const out = run("cargo", ["tree", "--offline", "-e", edges, "--target", TARGET, "--prefix", "none", "--format", "{p}"], tauriDir);
  const set = new Set<string>();
  for (const line of out.split("\n")) {
    const m = /^(\S+) v(\S+)/.exec(line);
    if (m) set.add(`${m[1]}@${m[2]}`);
  }
  return set;
}

const cargoMeta = JSON.parse(
  run("cargo", ["metadata", "--format-version", "1", "--offline", "--locked", "--manifest-path", join(tauriDir, "Cargo.toml")], tauriDir),
) as { packages: CargoPkg[] };
const linkedSet = treeSet("normal,no-proc-macro"); // really linked into the binary
const normalSet = treeSet("normal"); // + proc-macros and their deps (compile time only)
const byKey = new Map(cargoMeta.packages.map((p) => [`${p.name}@${p.version}`, p]));

const rustLinked: Component[] = [];
const rustBuildOnly: Component[] = [];
let ownCrate: CargoPkg | undefined;
for (const k of [...normalSet].sort()) {
  const p = byKey.get(k);
  if (!p) {
    flag(`cargo tree lists ${k} but cargo metadata has no such package`);
    continue;
  }
  if (p.source === null) {
    if (p.name === "openapp") ownCrate = p;
    else flag(`path dependency ${k} (${p.manifest_path}): third-party or own code? not in the notices`);
    continue;
  }
  if (p.source.startsWith("git+")) flag(`git dependency ${k} (${p.source})`);
  const linked = linkedSet.has(k);
  const license = p.license?.trim() ? p.license.trim() : null;
  const c: Component = {
    name: p.name,
    version: p.version,
    license,
    url: p.repository ?? p.homepage ?? `https://crates.io/crates/${p.name}/${p.version}`,
    cls: classifyExpr(license),
    copyrights: [],
    textKeys: [],
    standardText: false,
    noFile: false,
  };
  if (!license) flag(`no licence field: ${k}${p.license_file ? ` (license_file ${p.license_file})` : ""}`);
  if (license && /UNLICENSED|Proprietary|LicenseRef/i.test(license)) flag(`UNLICENSED/proprietary licence: ${k}: ${license}`);
  attachFiles(c, dirname(p.manifest_path), p.license_file, k);
  (linked ? rustLinked : rustBuildOnly).push(c);
}
if (!ownCrate) flag("cargo metadata did not report the openapp crate");

// ---- (b) npm ----------------------------------------------------------------------------------------------------

type PnpmLic = { name: string; versions: string[]; paths: string[]; license: string; homepage?: string; author?: string };
const pnpmOut = JSON.parse(run("pnpm", ["licenses", "list", "--prod", "--json"], root)) as Record<string, PnpmLic[]>;
const npmBundled: Component[] = [];
const npmTypesOnly: Component[] = [];
for (const list of Object.values(pnpmOut)) {
  for (const p of list) {
    p.versions.forEach((v, i) => {
      const dir = p.paths[i] ?? p.paths[0];
      let repo: string | undefined;
      try {
        const pj = JSON.parse(readFileSync(join(dir, "package.json"), "utf8")) as { repository?: string | { url?: string } };
        repo = typeof pj.repository === "string" ? pj.repository : pj.repository?.url;
        repo = repo?.replace(/^git\+/, "").replace(/\.git$/, "");
      } catch {
        /* ignore */
      }
      const c: Component = {
        name: p.name,
        version: v,
        license: p.license || null,
        url: p.homepage ?? repo ?? `https://www.npmjs.com/package/${p.name}/v/${v}`,
        cls: classifyExpr(p.license || null),
        copyrights: [],
        textKeys: [],
        standardText: false,
        noFile: false,
      };
      if (!p.license) flag(`npm package without licence field: ${p.name}@${v}`);
      attachFiles(c, dir, null, `${p.name}@${v}`);
      // type-only packages (.d.ts) never reach the bundle
      (p.name.startsWith("@types/") || p.name === "csstype" ? npmTypesOnly : npmBundled).push(c);
    });
  }
}
npmBundled.sort((a, b) => a.name.localeCompare(b.name));
npmTypesOnly.sort((a, b) => a.name.localeCompare(b.name));

// Standard-text fallback for components that ship no licence file (e.g. the microsandbox crates).
{
  const canonical = new Map<string, string>(); // title -> most used key
  const best = new Map<string, number>();
  for (const e of texts.values()) {
    if ((best.get(e.title) ?? 0) < e.users.size) {
      best.set(e.title, e.users.size);
      canonical.set(e.title, e.key);
    }
  }
  for (const c of [...rustLinked, ...rustBuildOnly, ...npmBundled]) {
    if (!c.noFile || !c.license) continue;
    for (const id of exprIds(c.license)) {
      const key = canonical.get(id);
      if (key && !c.textKeys.includes(key)) {
        c.textKeys.push(key);
        c.standardText = true;
        texts.get(key)?.users.add(`${c.name}@${c.version}`);
      }
    }
    if (c.textKeys.length) c.noFile = false; // text provided from the standard copy
  }
}

// ---- (c) bundled binaries ---------------------------------------------------------------------------------------

const MSB_SHA = "0995a03d0eaf46c7fadb28ea419be800def10f9152a12f0c03216bf4c8a0f06f";
const KRUNFW_SHA = "723f7cd73be9bab21c0abf6a9b07b45a07d4cd178f1690250e392d55dfb236b4";
const MSB_COMMIT = "7b7b9dc89e9a1c77801918f7833c3381d1754579"; // tag v0.7.8 (annotated tag object 3f72f34...)
const KRUNFW_COMMIT = "21f169f7e94798c8916315f8ac6f5999d5b88566"; // superradcompany/libkrunfw, vendor/libkrunfw submodule at MSB_COMMIT
const KERNEL_VERSION = "6.12.111";
const SOURCE_CONTACT = process.env.OPENAPP_SOURCE_CONTACT ?? "https://github.com/Martin-Atrin/openapp/issues (open an issue titled "Source request")";

function bundledPath(destKey: string): string | null {
  try {
    const conf = JSON.parse(readFileSync(join(tauriDir, "tauri.conf.json"), "utf8")) as { bundle?: { macOS?: { files?: Record<string, string> } } };
    const rel = conf.bundle?.macOS?.files?.[destKey];
    return rel ? resolve(tauriDir, rel) : null;
  } catch {
    return null;
  }
}
function checkHash(label: string, destKey: string, expected: string): "verified" | "mismatch" | "absent" {
  const p = bundledPath(destKey);
  if (!p || !existsSync(p)) {
    flag(`${label}: bundle input ${p ?? destKey} not found, sha256 not verified`);
    return "absent";
  }
  const got = sha256(readFileSync(p));
  if (got !== expected) {
    flag(`${label}: sha256 MISMATCH (${got} != ${expected}); the pinned source offer does not describe this file`);
    return "mismatch";
  }
  return "verified";
}
const msbCheck = checkHash("msb", "MacOS/msb", MSB_SHA);
const krunCheck = checkHash("libkrunfw", "Frameworks/libkrunfw.5.dylib", KRUNFW_SHA);

// Full GPL/LGPL texts for libkrunfw come from the libkrunfw repo at the pinned commit (cached in the repo).
const assetsDir = join(root, "scripts", "licenses-assets");
function assetText(name: string, user: string): string | null {
  const t = readText(join(assetsDir, name));
  if (t === null) {
    flag(`missing ${relative(root, join(assetsDir, name))} (fetch from github.com/superradcompany/libkrunfw @ ${KRUNFW_COMMIT})`);
    return null;
  }
  return addText(t, user);
}
const krunGpl = assetText("libkrunfw-LICENSE-GPL-2.0-only.txt", "libkrunfw (Linux kernel)");
const krunLgpl = assetText("libkrunfw-LICENSE-LGPL-2.1-only.txt", "libkrunfw (library)");

const msbCrate = rustLinked.find((c) => c.name === "microsandbox");
const msbCopyright = msbCrate ? [`Super Rad Company <development@superrad.company> (from the microsandbox crate metadata)`] : [];
if (!existsSync(join(root, "LICENSE")) && !existsSync(join(root, "LICENSE.md"))) {
  flag("no LICENSE file at the repo root, although src-tauri/Cargo.toml declares Apache-2.0 (add the Apache-2.0 text before a public release)");
}
const ownLicenseText = readText(join(root, "LICENSE")) ?? readText(join(root, "LICENSE.md"));
const ownKey = ownLicenseText ? addText(ownLicenseText, "OpenApp") : null;

// ---- (d) runtime downloads --------------------------------------------------------------------------------------

type DebPkg = { name: string; version: string; arch: string; licenses: string[]; hasCopyright: boolean };
let guest: { pkgs: DebPkg[]; node: { components: { name: string; path: string }[] } | null; imageVersion: string; baseImage: string; tarName: string } | null = null;

const dockerfile = readText(join(tauriDir, "vm", "guest-image", "Dockerfile")) ?? "";
const imageVersion = /org\.opencontainers\.image\.version="([^"]+)"/.exec(dockerfile)?.[1] ?? "unknown";
const baseImage = /^FROM\s+(\S+)/m.exec(dockerfile)?.[1] ?? "unknown";
const nodeVersion = /NODE_VERSION=(\S+)/.exec(readText(join(tauriDir, "vm", "guest-image", "setup.sh")) ?? "")?.[1] ?? "unknown";

function extractFromTar(outer: string, dest: string): void {
  const bsd = /bsdtar/.test(spawnSync("tar", ["--version"], { encoding: "utf8" }).stdout ?? "");
  const want = ["var/lib/dpkg/status", "usr/share/doc/*", "usr/local/LICENSE", "usr/lib/os-release"];
  const manifest = JSON.parse(execFileSync("tar", ["-xOf", outer, "manifest.json"], { encoding: "utf8" })) as { Layers: string[] }[];
  for (const layer of manifest[0]?.Layers ?? []) {
    // Only these paths are extracted from the layer; nothing is executed and nothing is booted.
    const pats = bsd ? want.flatMap((w) => ["--include", w, "--include", `./${w}`]) : ["--wildcards", ...want, ...want.map((w) => `./${w}`)];
    const r = spawnSync("sh", ["-c", `tar -xOf "$1" "$2" | tar -xf - -C "$3" ${pats.map((p) => `'${p}'`).join(" ")}`, "sh", outer, layer, dest], { encoding: "utf8" });
    if (!existsSync(join(dest, "var/lib/dpkg/status")) && r.status !== 0) throw new Error(`tar extraction failed: ${r.stderr.slice(0, 400)}`);
  }
}

function insideRoot(rootDir: string, p: string): string | null {
  try {
    const real = realpathSync(p);
    return real === rootDir || real.startsWith(rootDir + sep) ? real : null;
  } catch {
    return null;
  }
}

function debLicenses(text: string): string[] {
  const heads: string[] = [];
  for (const l of text.split("\n")) {
    const m = /^License:\s*(.*?)\s*$/.exec(l);
    if (m && m[1] && !heads.includes(m[1])) heads.push(m[1]);
  }
  if (heads.length) return heads;
  // not DEP-5: keyword guess, marked with a leading "~"
  const guess: string[] = [];
  const rules: [RegExp, string][] = [
    [/GNU Lesser General Public License|LGPL/i, "LGPL?"],
    [/GNU Affero/i, "AGPL?"],
    [/GNU General Public License|\bGPL\b/i, "GPL?"],
    [/Apache License/i, "Apache?"],
    [/Mozilla Public/i, "MPL?"],
    [/Redistribution and use in source and binary/i, "BSD?"],
    [/Permission is hereby granted, free of charge/i, "MIT?"],
    [/public domain/i, "public-domain?"],
  ];
  for (const [re, name] of rules) if (re.test(text)) guess.push(`~${name}`);
  return guess.length ? guess : ["~unparsed (read /usr/share/doc/<pkg>/copyright)"];
}

if (!has("--skip-guest")) {
  const tarPath = resolve(arg("--guest-tar") ?? join(process.env.HOME ?? "", ".openapp/vm/images", `openapp-guest-${imageVersion}.tar`));
  if (!existsSync(tarPath)) {
    flag(`guest image tar not found: ${tarPath} (guest-image-packages.txt not generated)`);
  } else {
    const tmp = mkdtempSync(join(tmpdir(), "openapp-licenses-"));
    try {
      extractFromTar(tarPath, tmp);
      const rootReal = realpathSync(tmp);
      const status = readFileSync(join(tmp, "var/lib/dpkg/status"), "utf8");
      const pkgs: DebPkg[] = [];
      for (const para of status.split(/\n\n+/)) {
        const f = new Map<string, string>();
        for (const line of para.split("\n")) {
          const m = /^([A-Za-z][A-Za-z0-9-]*):\s*(.*)$/.exec(line);
          if (m) f.set(m[1], m[2]);
        }
        if (!f.has("Package") || !/ installed$/.test(f.get("Status") ?? "")) continue;
        const name = f.get("Package") ?? "";
        const cp = insideRoot(rootReal, join(tmp, "usr/share/doc", name, "copyright"));
        const ctext = cp ? readText(cp) : null;
        pkgs.push({
          name,
          version: f.get("Version") ?? "?",
          arch: f.get("Architecture") ?? "?",
          licenses: ctext ? debLicenses(ctext) : ["(no copyright file)"],
          hasCopyright: ctext !== null,
        });
      }
      pkgs.sort((a, b) => a.name.localeCompare(b.name));
      const nodeLic = readText(join(tmp, "usr/local/LICENSE"));
      const components = nodeLic
        ? [...nodeLic.matchAll(/^- (.+?), located at (\S+?),/gm)].map((m) => ({ name: m[1], path: m[2] }))
        : [];
      guest = { pkgs, node: nodeLic ? { components } : null, imageVersion, baseImage, tarName: basename(tarPath) };

      const lines = [
        `# openapp-guest ${imageVersion} (${baseImage}): installed Debian packages. Generated by scripts/licenses.ts from /var/lib/dpkg/status in ${basename(tarPath)}.`,
        `# Columns (tab separated): package, version, architecture, licence. dpkg has no License field; the licence column is the`,
        `# short-name list from each package's /usr/share/doc/<package>/copyright (Debian DEP-5 "License:" stanzas). A leading "~" means the`,
        `# file is not DEP-5 and the name is a keyword guess. The copyright file is authoritative. Node.js ${nodeVersion} is installed from nodejs.org, not from dpkg.`,
        `# Source for these packages: deb.debian.org (suite trixie, deb-src), snapshot.debian.org for older point releases.`,
        ...pkgs.map((p) => `${p.name}\t${p.version}\t${p.arch}\t${p.licenses.slice(0, 12).join("; ")}${p.licenses.length > 12 ? `; ...(+${p.licenses.length - 12})` : ""}`),
      ];
      writeFileSync(join(outDir, "guest-image-packages.txt"), `${lines.join("\n")}\n`);
      if (pkgs.length === 0) flag("guest image: no installed packages parsed from dpkg status");
      const nocp = pkgs.filter((p) => !p.hasCopyright);
      if (nocp.length) flag(`guest image: ${nocp.length} packages without /usr/share/doc/<pkg>/copyright (${nocp.slice(0, 6).map((p) => p.name).join(", ")}${nocp.length > 6 ? ", ..." : ""})`);
    } finally {
      rmSync(tmp, { recursive: true, force: true }); // disk hygiene: the extracted files are regenerable
    }
  }
}

// Pinned runtime downloads that are not in the image. Versions come from the source so drift is visible.
const piVersion = /PI_VERSION: &str = "([^"]+)"/.exec(readText(join(tauriDir, "src", "pi.rs")) ?? "")?.[1] ?? "unknown";
const codexVersion = /CODEX_VERSION: &str = "([^"]+)"/.exec(readText(join(tauriDir, "src", "codex.rs")) ?? "")?.[1] ?? "unknown";
const PI_DEPS: [string, string, string][] = [
  ["diff", "8.0.4", "BSD-3-Clause"], ["jiti", "2.7.0", "MIT"], ["yaml", "2.9.0", "ISC"], ["chalk", "6.0.0", "MIT"],
  ["ignore", "7.0.8", "MIT"], ["semver", "7.8.5", "ISC"], ["undici", "8.10.2", "MIT"], ["typebox", "1.3.27", "MIT"],
  ["minimatch", "10.2.6", "BlueOak-1.0.0"], ["cross-spawn", "7.0.6", "MIT"], ["grok-mermaid", "0.2.3", "Apache-2.0"],
  ["highlight.js", "10.7.3", "BSD-3-Clause"], ["quickjs-wasi", "3.6.2", "MIT"], ["brace-expansion", "5.0.12", "MIT"],
  ["hosted-git-info", "9.0.3", "ISC"], ["proper-lockfile", "4.1.2", "MIT"], ["@earendil-works/pi-ai", "1.3.0", "MIT"],
  ["@earendil-works/pi-mcp", "1.3.0", "MIT"], ["@earendil-works/pi-tui", "1.3.0", "MIT"], ["@silvia-odwyer/photon-node", "0.3.4", "Apache-2.0"],
  ["@earendil-works/pi-codemode", "1.3.0", "MIT"], ["@earendil-works/pi-agent-core", "1.3.0", "MIT"],
]; // direct dependencies of @earendil-works/pi-coding-agent@1.3.0, licences from `npm view` (2026-10-17); transitive deps not enumerated
if (piVersion !== "1.3.0") flag(`PI_VERSION is ${piVersion}, notices describe 1.3.0: re-run the npm view checks and update PI_DEPS`);
if (has("--online")) {
  const view = (spec: string): string => {
    try {
      return execFileSync("npm", ["view", spec, "license"], { encoding: "utf8" }).trim();
    } catch {
      return "?";
    }
  };
  const pi = view(`@earendil-works/pi-coding-agent@${piVersion}`);
  const cx = view(`@openai/codex@${codexVersion}`);
  if (pi !== "MIT") flag(`npm view: Pi ${piVersion} licence is ${pi}, notices say MIT`);
  if (cx !== "Apache-2.0") flag(`npm view: Codex ${codexVersion} licence is ${cx}, notices say Apache-2.0`);
  for (const [n, v, l] of PI_DEPS) {
    const got = view(`${n}@${v}`);
    if (got !== l) flag(`npm view: ${n}@${v} licence is ${got}, notices say ${l}`);
  }
}

// ---- review flags from the data ---------------------------------------------------------------------------------

for (const c of rustLinked) {
  if (c.cls >= 1) flag(`${CLASS_NAME[c.cls]} in the release binary: ${c.name}@${c.version}: ${c.license ?? "(none)"}`);
  if (c.noFile) flag(`no licence file and no standard text available: ${c.name}@${c.version} (${c.license})`);
}
for (const c of npmBundled) {
  if (c.cls >= 1) flag(`${CLASS_NAME[c.cls]} in the shell bundle: ${c.name}@${c.version}: ${c.license ?? "(none)"}`);
  if (c.noFile) flag(`npm package without licence file: ${c.name}@${c.version}`);
}
for (const c of rustBuildOnly) {
  if (c.cls >= 2) flag(`build-time only (proc-macro tree), ${CLASS_NAME[c.cls]}: ${c.name}@${c.version}: ${c.license ?? "(none)"} (not distributed)`);
}
const standardOnly = rustLinked.filter((c) => c.standardText);
if (standardOnly.length) {
  flag(`${standardOnly.length} linked crates ship no licence file (standard text referenced, marked +): ${standardOnly.map((c) => c.name).slice(0, 8).join(", ")}${standardOnly.length > 8 ? ", ..." : ""}`);
}
if (guest) {
  const g = guest.pkgs.filter((p) => p.licenses.some((l) => /GPL|AGPL/i.test(l)));
  flag(`guest image: ${g.length}/${guest.pkgs.length} Debian packages carry GPL/LGPL terms; the image is hosted by the project (GitHub release), so source obligations for those packages apply to the project`);
}

// ---- output: markdown -------------------------------------------------------------------------------------------

const lockHash = (p: string): string => (existsSync(p) ? sha256(readFileSync(p)).slice(0, 12) : "missing");
const today = new Date().toISOString().slice(0, 10);
const esc = (s: string): string => s.replace(/\|/g, "\\|");

// Number texts: by title, then by first user.
const ordered = [...texts.values()].sort((a, b) => a.title.localeCompare(b.title) || b.users.size - a.users.size || a.firstUser.localeCompare(b.firstUser));
const textNo = new Map(ordered.map((e, i) => [e.key, `T${i + 1}`]));
const refs = (c: Component): string => c.textKeys.map((k) => textNo.get(k)).join(",") + (c.standardText ? "+" : "");

function summaryTable(list: Component[]): string {
  const counts = new Map<string, number>();
  for (const c of list) counts.set(c.license ?? "(none)", (counts.get(c.license ?? "(none)") ?? 0) + 1);
  const rows = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  return ["| Licence expression | Components |", "| --- | ---: |", ...rows.map(([l, n]) => `| ${esc(l)} | ${n} |`), `| **Total** | **${list.length}** |`].join("\n");
}
function classTable(): string {
  const groups: [string, Component[]][] = [["Rust crates (linked)", rustLinked], ["npm (shell bundle)", npmBundled]];
  const rows = groups.map(([label, list]) => {
    const n = [0, 0, 0, 0];
    for (const c of list) n[c.cls]++;
    return `| ${label} | ${list.length} | ${n[0]} | ${n[1]} | ${n[2]} | ${n[3]} |`;
  });
  return ["| Group | Total | Permissive | Weak copyleft | Strong copyleft | Unknown |", "| --- | ---: | ---: | ---: | ---: | ---: |", ...rows].join("\n");
}

function compactList(list: Component[]): string {
  const byExpr = new Map<string, Component[]>();
  for (const c of list) {
    const k = c.license ?? "(none)";
    byExpr.set(k, [...(byExpr.get(k) ?? []), c]);
  }
  const out: string[] = [];
  for (const [expr, cs] of [...byExpr.entries()].sort((a, b) => b[1].length - a[1].length || a[0].localeCompare(b[0]))) {
    out.push(`#### ${expr} (${cs.length})`, "");
    for (const c of cs) {
      const cp = c.copyrights.length ? ` — ${c.copyrights.join(" / ")}` : "";
      out.push(`- [${c.name}@${c.version}](${c.url})${cp} — ${refs(c) || "no text"}`);
    }
    out.push("");
  }
  return out.join("\n");
}

const guestCounts = new Map<string, number>();
if (guest) {
  for (const p of guest.pkgs) for (const l of new Set(p.licenses)) guestCounts.set(l, (guestCounts.get(l) ?? 0) + 1);
}
const guestTop = [...guestCounts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 14);

const md: string[] = [];
md.push(
  "# Third-Party Notices",
  "",
  "OpenApp is licensed under the Apache License 2.0.",
  "OpenApp includes or downloads the third-party components listed below. This file is generated by `scripts/licenses.ts`; do not edit it by hand.",
  "",
  `Generated ${today} from \`src-tauri/Cargo.lock\` (${lockHash(join(tauriDir, "Cargo.lock"))}), \`pnpm-lock.yaml\` (${lockHash(join(root, "pnpm-lock.yaml"))}), target \`${TARGET}\`.`,
  "Entries marked `+` use the standard licence text because the package ships no licence file. Licence texts appear once at the end (T1, T2, ...); copyright holders are on each entry. A package listed with several texts includes all of them.",
  "",
  "## 1. Summary",
  "",
  "Obligation class uses the easiest side of every `OR` expression.",
  "",
  classTable(),
  "",
  `### Rust crates linked into the release binary (${rustLinked.length})`,
  "",
  summaryTable(rustLinked),
  "",
  `### npm packages in the shell bundle (${npmBundled.length})`,
  "",
  summaryTable(npmBundled),
  "",
  "### Bundled binaries",
  "",
  "| Component | Licence | Form |",
  "| --- | --- | --- |",
  "| msb 0.7.8 (microsandbox) | Apache-2.0 | separate executable in `Contents/MacOS/` |",
  "| libkrunfw.5.dylib 5.6.1 | LGPL-2.1 (library) + GPL-2.0 (Linux 6.12.111 kernel) | separate dylib in `Contents/Frameworks/`, loaded by msb |",
  "| openapp-macos, OpenAppNotify.app | OpenApp's own code (Apache-2.0) | Swift executables, no third-party Swift packages |",
  "",
  "## 2. Source offer for GPL and LGPL components",
  "",
  "### libkrunfw.5.dylib (Linux kernel, GPL-2.0; library code, LGPL-2.1)",
  "",
  "OpenApp includes `Contents/Frameworks/libkrunfw.5.dylib`. This library embeds a Linux kernel (version " + KERNEL_VERSION + ", aarch64) that runs inside the OpenApp app VM. The kernel is licensed under the GNU General Public License version 2 (" + (krunGpl ? textNo.get(krunGpl) : "text missing") + "). The library glue code is licensed under the GNU Lesser General Public License version 2.1 (" + (krunLgpl ? textNo.get(krunLgpl) : "text missing") + "). The file is unmodified from the upstream release and is not linked into the OpenApp executable: the separate `msb` process loads it with `dlopen`.",
  "",
  "| Item | Value |",
  "| --- | --- |",
  `| File | \`libkrunfw.5.dylib\`, ${krunCheck === "verified" ? "sha256 verified against the bundle input" : "sha256 NOT verified against the bundle input"}: \`${KRUNFW_SHA}\` |`,
  `| Upstream binary | microsandbox release v0.7.8, asset \`libkrunfw-darwin-aarch64.dylib\` (same sha256), https://github.com/superradcompany/microsandbox/releases/tag/v0.7.8 |`,
  `| Release commit | microsandbox \`${MSB_COMMIT}\` (tag v0.7.8) |`,
  `| Corresponding source | https://github.com/superradcompany/libkrunfw at commit \`${KRUNFW_COMMIT}\` (the \`vendor/libkrunfw\` submodule of that release commit). It holds the Makefile (libkrunfw 5.6.1), the kernel configuration \`config-libkrunfw_aarch64\`, the kernel patches in \`patches/\`, and the build scripts. |`,
  `| Kernel source | linux-${KERNEL_VERSION}: https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.gz (the unpatched tarball that the Makefile downloads) |`,
  "| Check | The library's embedded version banner reads `Linux version 6.12.111 ... #1 SMP Fri Jul 24 13:01:52 WAT 2026`, which equals `KERNEL_VERSION` and `TIMESTAMP` in that commit's Makefile. The build was not reproduced bit for bit. |",
  "",
  "Written offer. For three years from the date you received OpenApp, we will give any third party, on request and for a charge not more than the cost of physically performing the distribution, a complete machine-readable copy of the corresponding source code of libkrunfw " + "5.6.1 and the Linux kernel " + KERNEL_VERSION + " as listed above, including the kernel configuration and patches. Request it at: " + SOURCE_CONTACT + ". You can also take the source from the links above at no cost.",
  "",
  "You may replace `libkrunfw.5.dylib` with your own build of the same ABI (soname version 5). OpenApp passes the path of the bundled file to `msb` explicitly. Re-signing the app bundle is needed after replacement.",
  "",
  "### Debian packages in the guest image",
  "",
  "The guest image downloaded at first run (section 6) contains Debian packages under GPL, LGPL and other licences. Source for every package: Debian `trixie` source packages at https://deb.debian.org/debian (deb-src), and https://snapshot.debian.org for exact versions. The exact package list with versions is in `guest-image-packages.txt`. The written offer above applies to these packages too.",
  "",
  "## 3. Bundled binaries",
  "",
  "### msb 0.7.8 (microsandbox runtime)",
  "",
  `- Licence: Apache-2.0 (${(msbCrate ? msbCrate.textKeys.map((k) => textNo.get(k)).join(",") : "text in appendix") || "text in appendix"}${msbCrate?.standardText ? "+" : ""}). Copyright: ${msbCopyright.join("; ") || "see upstream"}.`,
  "- URL: https://github.com/superradcompany/microsandbox (release v0.7.8, asset `msb-darwin-aarch64`)",
  `- sha256 \`${MSB_SHA}\` (${msbCheck === "verified" ? "verified against the bundle input" : "not verified against the bundle input"}).`,
  "- The executable is statically linked against Rust libraries of the microsandbox workspace, including the libkrun fork `msb_krun` 0.1.41 (Apache-2.0). The SDK crates linked into OpenApp's own binary are in section 4. The complete crate list of the `msb` executable itself comes from the microsandbox workspace `Cargo.lock` at the release commit above. UNVERIFIED: that list is not yet reproduced here crate by crate.",
  "",
  "### libkrunfw.5.dylib 5.6.1",
  "",
  `- Licence: LGPL-2.1 (${krunLgpl ? textNo.get(krunLgpl) : "text missing"}) for the library, GPL-2.0 (${krunGpl ? textNo.get(krunGpl) : "text missing"}) for the Linux kernel. Source offer: section 2. URL: https://github.com/superradcompany/libkrunfw.`,
  "",
  "### openapp-macos and OpenAppNotify.app (Swift sidecar)",
  "",
  "- OpenApp's own code (`src-tauri/macos-services`), Apache-2.0. It uses only Apple system frameworks and the Swift runtime of macOS; there are no third-party Swift packages.",
  "",
  `## 4. Rust crates linked into the release binary (${rustLinked.length})`,
  "",
  "Normal dependencies of `src-tauri/Cargo.toml` for `aarch64-apple-darwin` (no dev, build-script or proc-macro crates). Grouped by licence expression; each entry shows name@version (crate repository), copyright line when the licence file has one, and licence text numbers.",
  "",
  compactList(rustLinked),
  `## 5. npm packages in the shell bundle (${npmBundled.length})`,
  "",
  "Production dependencies of the Vite shell (`pnpm licenses list --prod`). They are bundled into `dist/`, which the app binary embeds.",
  "",
  compactList(npmBundled),
);
if (npmTypesOnly.length) {
  md.push(`Type-only packages (TypeScript declarations, not part of the bundle): ${npmTypesOnly.map((c) => `${c.name}@${c.version} (${c.license})`).join(", ")}.`, "");
}

md.push("## 6. Downloaded at runtime (not distributed with the app)", "", "These components are not in the `.app`. OpenApp downloads or installs them after the user agrees, and they are listed here for transparency.", "");
md.push("### Guest image (Debian trixie)", "");
if (guest) {
  md.push(
    `- \`openapp-guest\` ${guest.imageVersion}, base \`${guest.baseImage}\`, ${guest.pkgs.length} installed Debian packages. Full list with versions and licences: \`guest-image-packages.txt\` (read from \`/var/lib/dpkg/status\` and \`/usr/share/doc/*/copyright\` in \`${guest.tarName}\`; the image was not booted).`,
    "- Main licences by package count (a package can have several): " + guestTop.map(([l, n]) => `${l} ${n}`).join(", ") + ".",
    "- Debian packages are separate programs inside the VM and keep their own licences (many are GPL or LGPL; see section 2 for source).",
  );
} else {
  md.push("- Package list not generated in this run (`--skip-guest` or image tar missing).");
}
md.push(
  "",
  `### Node.js ${nodeVersion}`,
  "",
  "- Licence: MIT, plus the licences of bundled dependencies; the complete text is the `LICENSE` file of the Node.js distribution (https://github.com/nodejs/node/blob/main/LICENSE). Installed into the guest image from https://nodejs.org/dist/ with a pinned sha256.",
  guest?.node ? `- Bundled components named in that LICENSE (at least ${guest.node.components.length}): ${guest.node.components.map((c) => c.name).join(", ")}.` : "- Bundled component list not generated in this run.",
  "",
  `### Pi coding agent ${piVersion} (\`@earendil-works/pi-coding-agent\`)`,
  "",
  "- Licence: MIT. URL: https://github.com/earendil-works/pi. Installed with npm into the `/opt/openapp` volume at first use.",
  `- Direct dependencies and licences (npm metadata): ${PI_DEPS.map(([n, v, l]) => `${n}@${v} ${l}`).join("; ")}. Transitive dependencies are not enumerated here. UNVERIFIED: transitive licences.`,
  "",
  `### Codex ${codexVersion} (\`@openai/codex\`)`,
  "",
  "- Licence: Apache-2.0. URL: https://github.com/openai/codex. Gated provider; not used in the release.",
  "",
);
if (rustBuildOnly.length) {
  md.push(
    "## 7. Build-time only",
    "",
    `${rustBuildOnly.length} further crates are compiled only to build the app (procedural macros and their dependencies). They are not part of the distributed binary and are listed for transparency: ` +
      rustBuildOnly.map((c) => `${c.name}@${c.version} (${c.license ?? "none"})`).join(", ") +
      ".",
    "",
  );
}

md.push("## Appendix: licence texts", "");
for (const e of ordered) {
  const no = textNo.get(e.key);
  const variants = [...e.variants.values()];
  let body = variants[0];
  if (variants.length > 1) {
    // copyright lines differ per component: keep the permission text, point to the entries for holders
    const end = /END OF TERMS AND CONDITIONS/i.exec(body);
    if (end) body = body.slice(0, end.index + end[0].length);
    body = body
      .split("\n")
      .map((l) => (isHolderLine(l) ? l.replace(/^(\s*).*$/, "$1Copyright (c) <holder>: see the component entries") : l))
      .join("\n")
      .replace(/(Copyright \(c\) <holder>: see the component entries\n)+/g, "Copyright (c) <holder>: see the component entries\n");
  }
  const users = [...e.users];
  const sample = users.slice(0, 3).join(", ") + (users.length > 3 ? `, and ${users.length - 3} more` : "");
  md.push(`### ${no}: ${e.title}`, "", `Used by ${users.length} component${users.length === 1 ? "" : "s"}: ${sample}.`, "", "~~~~text", body, "~~~~", "");
}

const mdText = md.join("\n");
writeFileSync(join(outDir, "THIRD-PARTY-NOTICES.md"), `${mdText}\n`);

// ---- console report ---------------------------------------------------------------------------------------------

const countBy = (list: Component[]): string =>
  [...list.reduce((m, c) => m.set(c.license ?? "(none)", (m.get(c.license ?? "(none)") ?? 0) + 1), new Map<string, number>()).entries()]
    .sort((a, b) => b[1] - a[1])
    .map(([l, n]) => `${n} ${l}`)
    .join("; ");
console.log(`rust linked: ${rustLinked.length}  build-time only: ${rustBuildOnly.length}  npm bundled: ${npmBundled.length}  distinct texts: ${texts.size}`);
console.log(`rust licences: ${countBy(rustLinked)}`);
console.log(`npm licences: ${countBy(npmBundled)}`);
if (guest) console.log(`guest packages: ${guest.pkgs.length}; top licences: ${guestTop.map(([l, n]) => `${n} ${l}`).join("; ")}`);
console.log(`wrote ${join(outDir, "THIRD-PARTY-NOTICES.md")}${guest ? ` and ${join(outDir, "guest-image-packages.txt")}` : ""}`);
console.log(flags.length ? `\nREVIEW FLAGS (${flags.length}):\n${flags.map((f) => `  - ${f}`).join("\n")}` : "\nno review flags");
