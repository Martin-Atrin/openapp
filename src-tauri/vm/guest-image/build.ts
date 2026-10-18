// Build the OpenApp guest image and export it as a docker-archive tar that the kernel loads into
// microsandbox on first start (`Image::load`).
// Usage: node src-tauri/vm/guest-image/build.ts [--builder msb|docker] [--out <tar>]
// Default output: $OPENAPP_VM_HOME/images/openapp-guest-<version>.tar (OPENAPP_VM_HOME defaults to ~/.openapp/vm).
//
// Builders (both run setup.sh on the pinned Debian base):
// - msb (default): a throwaway microsandbox VM with network (the same runtime the app uses; no Docker).
//   State in $OPENAPP_VM_BUILD_HOME (default ~/.openapp/vm-build; delete it after a build if disk is tight).
//   The rootfs is tarred inside the VM (one layer, zstd) and wrapped into a docker-archive here.
// - docker: `docker build` of the Dockerfile + `docker save` (needs ~4 GB free in the Docker VM).
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Keep in sync with GUEST_IMAGE_VERSION in src-tauri/src/vm.rs and the label in Dockerfile/setup.sh. */
const IMAGE_VERSION = "0.9.2";
const BASE = "debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f";
const ENV = [
  "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
  "LANG=C.UTF-8",
  "NPM_CONFIG_UPDATE_NOTIFIER=false",
  "NPM_CONFIG_FUND=false",
  "NODE_USE_ENV_PROXY=1",
  "PIP_DISABLE_PIP_VERSION_CHECK=1",
];
const tag = `openapp-guest:${IMAGE_VERSION}`;
const here = dirname(fileURLToPath(import.meta.url));
const vmHome = process.env.OPENAPP_VM_HOME || join(homedir(), ".openapp", "vm");
const arg = (name: string) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : undefined;
};
const out = resolve(arg("--out") ?? join(vmHome, "images", `openapp-guest-${IMAGE_VERSION}.tar`));
const builder = arg("--builder") ?? "msb";
const t0 = Date.now();
mkdirSync(dirname(out), { recursive: true });

const sha256File = (p: string) =>
  new Promise<string>((ok, fail) => {
    const h = createHash("sha256");
    createReadStream(p).on("data", (d) => h.update(d)).on("end", () => ok(h.digest("hex"))).on("error", fail);
  });

if (builder === "docker") {
  const run = (args: string[]) => execFileSync("docker", args, { stdio: "inherit" });
  run(["build", "--platform", `linux/${process.arch === "x64" ? "amd64" : "arm64"}`, "-t", tag, here]);
  run(["save", "-o", out, tag]);
} else if (builder === "msb") {
  const repo = resolve(here, "..", "..", "..");
  const msbDir = process.env.OPENAPP_MSB_DIR || join(repo, "src-tauri", "vm", "runtime");
  const home = process.env.OPENAPP_VM_BUILD_HOME || join(homedir(), ".openapp", "vm-build");
  mkdirSync(home, { recursive: true });
  for (const d of ["bin", "lib"]) {
    const link = join(home, d);
    // re-point links left from an older runtime location (spikes/vm/.msb before 2026-10-17)
    if (lstatSync(link, { throwIfNoEntry: false })?.isSymbolicLink() && readlinkSync(link) !== join(msbDir, d)) rmSync(link);
    if (!existsSync(link)) symlinkSync(join(msbDir, d), link);
  }
  const work = join(home, "out");
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const name = "oa-guest-build";
  // Inside the VM: setup.sh, then one zstd layer of the rootfs (the VM's own mounts and the files
  // microsandbox writes at boot are left out) plus the uncompressed tar's sha256 (diff_id).
  const script = [
    "set -eu",
    "sh /src/setup.sh",
    "cd /",
    "tar --numeric-owner --one-file-system -cpf /out/layer.tar --exclude=./src --exclude=./out --exclude=./.msb " +
      "--exclude=./etc/hostname --exclude=./etc/hosts --exclude=./etc/resolv.conf --exclude='./tmp/*' --exclude=./root/.npm --exclude='./var/cache/debconf/*-old' .",
    "sha256sum /out/layer.tar | cut -d' ' -f1 > /out/diff_id",
    "zstd -q -T0 -12 --rm /out/layer.tar -o /out/layer.tar.zst",
    "echo BUILD_OK",
  ];
  const msb = join(home, "bin", "msb");
  execFileSync(
    msb,
    ["run", "--replace", "-n", name, "-c", "4", "-m", "4096", "--mount-dir", `${here}:/src`, "--mount-dir", `${work}:/out`, BASE, "--", "bash", "-c", script.join("\n")],
    { stdio: "inherit", env: { ...process.env, MSB_HOME: home } },
  );
  try {
    execFileSync(msb, ["rm", name], { stdio: "ignore", env: { ...process.env, MSB_HOME: home } });
  } catch {}
  const diffId = readFileSync(join(work, "diff_id"), "utf8").trim();
  const layerPath = join(work, "layer.tar.zst");
  const layerSha = await sha256File(layerPath);
  const config = {
    architecture: process.arch === "x64" ? "amd64" : "arm64",
    os: "linux",
    created: new Date().toISOString(),
    config: {
      Env: ENV,
      Cmd: ["bash"],
      Labels: {
        "org.opencontainers.image.title": "openapp-guest",
        "org.opencontainers.image.version": IMAGE_VERSION,
        "org.opencontainers.image.base.name": BASE,
      },
    },
    rootfs: { type: "layers", diff_ids: [`sha256:${diffId}`] },
    history: [{ created_by: "src-tauri/vm/guest-image/setup.sh (msb builder)" }],
  };
  const cfgBytes = Buffer.from(JSON.stringify(config));
  const cfgSha = createHash("sha256").update(cfgBytes).digest("hex");
  const stage = join(work, "archive");
  mkdirSync(join(stage, "blobs", "sha256"), { recursive: true });
  writeFileSync(join(stage, "blobs", "sha256", cfgSha), cfgBytes);
  execFileSync("mv", [layerPath, join(stage, "blobs", "sha256", layerSha)]);
  writeFileSync(
    join(stage, "manifest.json"),
    JSON.stringify([{ Config: `blobs/sha256/${cfgSha}`, RepoTags: [tag], Layers: [`blobs/sha256/${layerSha}`] }]),
  );
  execFileSync("tar", ["-cf", out, "-C", stage, "manifest.json", "blobs"], { stdio: "inherit" });
  rmSync(work, { recursive: true, force: true });
} else {
  throw new Error(`unknown builder ${builder} (msb | docker)`);
}
const mb = (statSync(out).size / 1e6).toFixed(1);
console.log(`${tag} -> ${out} (${mb} MB, ${((Date.now() - t0) / 1000).toFixed(1)} s, builder ${builder})`);
