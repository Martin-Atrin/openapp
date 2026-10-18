// §10 host data binding: any spec prop may be { "$host": "<source>" }, resolved from the snapshot before render.
// §21 G1: { "$stream": "<topic>", "path"?: "a.b", "default"? } resolves from the latest channel message.
import type { ChannelMessage, Snapshot, Timestamp } from "./contract";

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);

function toMs(t: Timestamp | undefined): number | undefined {
  if (typeof t === "number") return t < 1e11 ? t * 1000 : t; // seconds vs ms
  if (typeof t === "string") {
    const n = Date.parse(t);
    return Number.isNaN(n) ? undefined : n;
  }
  return undefined;
}

export function relativeTime(t: Timestamp | undefined, now = Date.now()): string {
  const ms = toMs(t);
  if (ms === undefined) return "";
  const diff = Math.max(0, now - ms);
  const min = Math.floor(diff / 60000);
  if (min < 1) return "just now";
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  const d = new Date(ms);
  const today = new Date(now);
  const startOfDay = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((startOfDay(today) - startOfDay(d)) / 86400000);
  if (days === 0) return `${hr}h ago`;
  if (days === 1) return "yesterday";
  if (days < 7) return `${days}d ago`;
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

const GRANT_LABEL: Record<string, string> = {
  network: "Network", "web.control": "Web control", microphone: "Microphone",
  "clipboard.read": "Clipboard read", "shortcut.global": "Global shortcuts", camera: "Camera", folder: "Folder",
};
const GRANT_ICON: Record<string, string> = {
  network: "globe", "web.control": "mouse-pointer-click", microphone: "mic",
  "clipboard.read": "clipboard", "shortcut.global": "keyboard", camera: "camera", folder: "folder",
};

function source(name: string, snap: Snapshot): unknown {
  switch (name) {
    case "sessions":
      return (snap.sessions ?? []).map((s) => ({
        id: s.id,
        title: s.title || "New chat",
        subtitle: relativeTime(s.updated),
        icon: "message-square",
        active: s.id === snap.activeSession,
        action: { host: "session.open", id: s.id },
      }));
    case "harnesses":
      return (snap.harnesses ?? []).map((h) => ({
        id: h.id,
        title: h.name,
        subtitle: h.note ?? (h.available ? "Available" : "Not installed"),
        icon: "bot",
        active: h.id === snap.agent.harness,
        action: { host: "harness.select", id: h.id },
      }));
    case "grants":
      return (snap.grants ?? []).map((g) => ({
        id: g.id,
        title: g.target,
        subtitle: `${GRANT_LABEL[g.kind] ?? g.kind} · ${g.kind === "folder" ? "mounted in the VM" : g.allowed ? (g.scope === "workspace" ? "allowed for this workspace" : "allowed") : "denied"}`,
        icon: GRANT_ICON[g.kind] ?? "shield",
        action: { host: "grant.revoke", id: g.id },
      }));
    case "secrets":
      return (snap.secrets ?? []).map((x) => ({
        id: x.name,
        title: x.label || x.name,
        subtitle: [
          x.kind === "oauth-token" ? "Connected account" : x.kind === "oauth-client" ? "OAuth client" : x.kind === "password" ? "Password" : "Token",
          x.provider ?? undefined,
          x.hosts?.length ? x.hosts.join(", ") : undefined,
        ].filter(Boolean).join(" · "),
        icon: "key-round",
        action: { host: "secret.delete", name: x.name },
      }));
    case "session.active.title":
      return snap.sessions?.find((s) => s.id === snap.activeSession)?.title || "New chat";
    case "agent.name":
      return snap.agent.name;
    case "agent.status":
      return snap.agent.status;
    default:
      if (name.startsWith("web:")) {
        return snap.web?.[name.slice(4)] ?? { url: "", title: "", loading: false };
      }
      // §21 registry primitives (channels, resources, safe_mode, …)
      if (snap.hostData && Object.prototype.hasOwnProperty.call(snap.hostData, name)) return snap.hostData[name];
      throw new Error(`Unknown $host source "${name}"`);
  }
}

export type Streams = Record<string, ChannelMessage | undefined>;

/** `$stream` binding → value at `path` of the latest message's data (base64 string for binary), else `default`. */
export function resolveStream(binding: Obj, last: ChannelMessage | undefined): unknown {
  const dflt = binding.default ?? null;
  if (!last) return dflt;
  let v: unknown = last.data !== undefined ? last.data : last.b64;
  const path = typeof binding.path === "string" ? binding.path : "";
  if (path) {
    for (const seg of path.split(".")) {
      if (Array.isArray(v)) v = v[Number(seg)];
      else if (isObj(v)) v = v[seg];
      else return dflt;
      if (v === undefined) return dflt;
    }
  }
  return v === null || v === undefined ? dflt : v;
}

/** Topics bound with `$stream` in a node's props (not its children). */
export function streamTopics(node: Obj): string[] {
  const out = new Set<string>();
  const walk = (v: unknown) => {
    if (Array.isArray(v)) return v.forEach(walk);
    if (!isObj(v)) return;
    if (typeof v.$stream === "string") return void out.add(v.$stream);
    Object.values(v).forEach(walk);
  };
  for (const [k, v] of Object.entries(node)) if (k !== "children") walk(v);
  return [...out].sort();
}

function resolveValue(v: unknown, snap: Snapshot, errors: string[], streams: Streams): unknown {
  if (Array.isArray(v)) return v.map((x) => resolveValue(x, snap, errors, streams));
  if (!isObj(v)) return v;
  if (typeof v.$stream === "string") return resolveStream(v, streams[v.$stream]);
  if (typeof v.$host === "string") {
    try {
      return source(v.$host, snap);
    } catch (e) {
      errors.push((e as Error).message);
      return undefined;
    }
  }
  const out: Obj = {};
  for (const [k, x] of Object.entries(v)) out[k] = resolveValue(x, snap, errors, streams);
  return out;
}

/** Resolve $host in every prop of a node except `children` (child nodes resolve themselves). */
export function resolveHostProps(node: Obj, snap: Snapshot, streams: Streams = {}): { node: Obj; errors: string[] } {
  const errors: string[] = [];
  const out: Obj = {};
  for (const [k, v] of Object.entries(node)) out[k] = k === "children" ? v : resolveValue(v, snap, errors, streams);
  return { node: out, errors };
}
