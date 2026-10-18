// In-memory dev kernel: lets the shell be built and tested in a plain browser (no Tauri).
// Try prompts: "demo", "html", "theme", "permission", "broken", "/undo", "/reset", anything else is echoed.
import type {
  Action,
  ActionResult,
  CallToolResult,
  ChannelMessage,
  ChatEvent,
  ChatItem,
  HarnessInfo,
  ComposerEvent,
  ModelChoice,
  ModelInfo,
  ModelPrefs,
  NotifyEvent,
  Mode,
  Question,
  SessionMeta,
  Snapshot,
  ThemeValue,
  ViewDef,
  WebRect,
} from "./contract";
import type { Kernel, Unsub } from "./kernel";

const DEFAULT_THEME: Record<string, ThemeValue> = {
  bg: "#fbfaf8",
  surface: "#ffffff",
  "surface-2": "#f3f1ed",
  text: "#1d1c1a",
  "text-muted": "#77736c",
  accent: "#4457d6",
  "accent-text": "#ffffff",
  border: "#e6e2db",
  success: "#2f8f5b",
  warning: "#b7791f",
  danger: "#c2413b",
  radius: 12,
  density: "normal",
};

const PALETTES: { name: string; tokens: Record<string, ThemeValue> }[] = [
  {
    name: "ink",
    tokens: {
      bg: "#0f1115", surface: "#171a21", "surface-2": "#1f232c", text: "#eceef2", "text-muted": "#8b93a3",
      accent: "#8aa4ff", "accent-text": "#0b0d12", border: "#2a2f3a", radius: 14,
    },
  },
  {
    name: "sand",
    tokens: {
      bg: "#f4ecdf", surface: "#fbf6ee", "surface-2": "#ece1cf", text: "#2b2217", "text-muted": "#85735b",
      accent: "#b4532a", "accent-text": "#ffffff", border: "#dccdb4", radius: 4, density: "comfy",
    },
  },
  { name: "default", tokens: { ...DEFAULT_THEME } },
];

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const clone = <T>(v: T): T => JSON.parse(JSON.stringify(v)) as T;
let uid = 0;
const nid = (p: string) => `${p}_${(++uid).toString(36)}${Math.random().toString(16).slice(2, 6)}`;

const MIN = 60_000;
function seedSessions(): { sessions: SessionMeta[]; chats: Record<string, ChatItem[]> } {
  const now = Date.now();
  const cur = "s_" + Math.random().toString(16).slice(2, 10);
  const a = "s_" + Math.random().toString(16).slice(2, 10);
  const b = "s_" + Math.random().toString(16).slice(2, 10);
  return {
    sessions: [
      { id: cur, title: "New chat", created: now, updated: now, mode: "chat" },
      { id: a, title: "Add a pomodoro timer", created: now - 40 * MIN, updated: now - 2 * MIN },
      { id: b, title: "Make it more colourful and add a sessions rail", created: now - 30 * 3600_000, updated: now - 26 * 3600_000 },
    ],
    chats: {
      [cur]: [],
      [a]: [
        { id: "a1", kind: "user", text: "Add a pomodoro timer" },
        { id: "a2", kind: "agent", text: "Added a **timer** module in the main column." },
      ],
      [b]: [
        { id: "b1", kind: "user", text: "Make it more colourful and add a sessions rail" },
        { id: "b2", kind: "agent", text: "Done: new theme plus a left rail." },
      ],
    },
  };
}

const HARNESSES: HarnessInfo[] = [
  { id: "codex", name: "Codex · isolated VM", adapter: "codex-app-server", available: true, signedIn: false, vm: true, note: "Sign in required: Codex in the app VM has its own login", login: { state: "required" } },
  {
    id: "pi", name: "Pi · isolated VM", adapter: "pi-rpc", available: true, signedIn: false, vm: true, note: "Sign in required: Pi in the app VM has its own login",
    login: { state: "required", methods: [
      { id: "chatgpt", label: "Sign in with ChatGPT" },
      { id: "claude", label: "Sign in with Claude", note: "Anthropic may restrict the use of Claude subscriptions in third-party apps." },
      { id: "apikey", label: "Use an API key", providers: [{ id: "openai", label: "OpenAI" }, { id: "anthropic", label: "Anthropic" }] },
    ] },
  },
  { id: "claude", name: "Claude Code", adapter: "acp", available: true, signedIn: null, note: "Login could not be verified" },
  { id: "gemini", name: "Gemini CLI", adapter: "acp", available: false, signedIn: null, note: "Not installed" },
];

/** `?gated=1` previews release gating: only Pi is usable, the rest are "Coming soon". */
const GATED = typeof location !== "undefined" && new URLSearchParams(location.search).get("gated") === "1";
const SHOWN_HARNESSES: HarnessInfo[] = GATED
  ? HARNESSES.map((h) => (h.id === "pi" ? h : { id: h.id, name: h.name, adapter: h.adapter, available: false, signedIn: null, note: "Coming soon", comingSoon: true, ...(h.vm ? { vm: true } : {}) }))
  : HARNESSES;

function initial(): Snapshot {
  const { sessions, chats } = seedSessions();
  seeded = chats;
  return {
    sessions,
    activeSession: sessions[0]!.id,
    layout: { regions: [], placements: [] },
    theme: { ...DEFAULT_THEME },
    modules: [],
    views: {},
    harnesses: SHOWN_HARNESSES,
    activeMode: "chat",
    web: {},
    presented: [],
    grants: [{ id: "network:github.com", kind: "network", target: "github.com", allowed: true, scope: "workspace" }],
    agent: { name: "No provider", status: "unselected", harness: null },
    chat: [],
  };
}

let seeded: Record<string, ChatItem[]> = {};

/** §23 obviously fake catalog: the mock "discovers" it for whichever provider is picked. */
function fakeModels(harness: string): ModelInfo[] {
  return [
    { id: "mock-sparrow", displayName: "Mock Sparrow", description: "Fake fast model (mock kernel).", efforts: ["low", "medium", "high", "xhigh"], defaultEffort: "medium", isDefault: true, harness },
    { id: "mock-heron", displayName: "Mock Heron", description: "Fake workhorse model (mock kernel).", efforts: ["low", "medium", "high", "xhigh", "max"], defaultEffort: "medium", harness },
    { id: "mock-condor", displayName: "Mock Condor", description: "Fake frontier model (mock kernel).", efforts: ["medium", "high", "xhigh", "max"], defaultEffort: "high", harness },
  ];
}

function notesView(notes: string[]): ViewDef {
  return {
    kind: "spec",
    tree: {
      type: "stack",
      gap: 12,
      padding: 14,
      children: [
        { type: "input", name: "note", placeholder: "Add a note and press Enter", submit: { tool: "add_note" } },
        {
          type: "list",
          empty: "No notes yet.",
          items: notes.map((n, i) => ({
            id: String(i),
            title: n,
            subtitle: "just now",
            icon: "sticky-note",
            action: { host: "composer.insert", text: n },
          })),
        },
      ],
    },
  };
}

function overviewView(s: { toggled: boolean; progress: number }): ViewDef {
  return {
    kind: "spec",
    tree: {
      type: "card",
      title: "Hello from a module",
      children: [
        { type: "text", value: "This card is a **spec view** rendered natively by the shell, themed by --oa-* variables.", variant: "muted" },
        {
          type: "stack", direction: "row", gap: 8, children: [
            { type: "badge", label: "neutral" },
            { type: "badge", label: "accent", tone: "accent" },
            { type: "badge", label: "success", tone: "success" },
            { type: "badge", label: "warning", tone: "warning" },
            { type: "badge", label: "danger", tone: "danger" },
          ],
        },
        { type: "progress", value: s.progress },
        { type: "toggle", label: "Notifications", value: s.toggled, action: { tool: "toggle_flag" } },
        {
          type: "stack", direction: "row", gap: 8, children: [
            { type: "button", label: "Slow tool", icon: "timer", variant: "primary", action: { tool: "ping" } },
            { type: "button", label: "Ask the agent", icon: "sparkles", variant: "secondary", action: { host: "prompt", text: "Make the accent colour warmer" } },
            { type: "button", label: "Insert text", variant: "ghost", action: { host: "composer.insert", text: "Add a pomodoro timer " } },
            { type: "button", label: "Fail", variant: "danger", action: { tool: "fail" } },
          ],
        },
        { type: "divider" },
        { type: "slot", id: "extras" },
      ],
    },
  };
}

const MOCK_HTML: ViewDef = { kind: "html", url: "/mock/panel.html", rev: 1 };

export class MockKernel implements Kernel {
  readonly isMock = true;
  private snap: Snapshot = initial();
  private chats: Record<string, ChatItem[]> = seeded;
  private history: { layout: Snapshot["layout"]; theme: Snapshot["theme"] }[] = [];
  private notes = ["Buy oat milk", "Ship the shell"];
  private flags = { toggled: false, progress: 0.35, mic: false };
  private paletteIdx = 0;
  private turn = 0;
  private cancelled = false;
  private permResolve: ((v: string | null) => void) | null = null;
  private askResolve: ((a: Record<string, string[]> | null) => void) | null = null;
  private snapCbs = new Set<(s: Snapshot) => void>();
  private chatCbs = new Set<(e: ChatEvent) => void>();
  private compCbs = new Set<(e: ComposerEvent) => void>();
  private notifyCbs = new Set<(e: NotifyEvent) => void>();

  // ---- §23 models (fake catalog) ----
  private modelPrefs: { chat?: ModelChoice; build?: ModelChoice; strong?: ModelChoice; policy: "ask" | "auto" } = { policy: "ask" };
  private decorateModels() {
    const h = this.snap.agent.harness;
    if (!h || this.snap.agent.status === "unselected") {
      this.snap.models = [];
      this.snap.modelPrefs = null;
      delete this.snap.agent.model;
      delete this.snap.agent.effort;
      delete this.snap.agent.modelName;
      return;
    }
    const cat = fakeModels(h);
    const mode = this.snap.activeMode ?? "chat";
    const choice = mode === "build" ? (this.modelPrefs.build ?? this.modelPrefs.chat) : this.modelPrefs.chat;
    const m = cat.find((x) => x.id === choice?.model) ?? cat.find((x) => x.isDefault) ?? cat[0]!;
    const effort = choice?.model === m.id && choice.effort && m.efforts.includes(choice.effort) ? choice.effort : m.defaultEffort ?? null;
    this.snap.models = cat;
    Object.assign(this.snap.agent, { model: m.id, effort, modelName: m.displayName, tier: "default" });
    const prefs: ModelPrefs = {
      harness: h, chat: this.modelPrefs.chat ?? null, build: this.modelPrefs.build ?? null, strong: this.modelPrefs.strong ?? null,
      policy: this.modelPrefs.policy, source: "mock", fetchedAt: new Date().toISOString(),
      current: { model: m.id, effort, tier: "default", label: `${m.displayName}${effort ? ` · ${effort}` : ""}` },
    };
    this.snap.modelPrefs = prefs;
  }
  private setModel(a: Record<string, unknown>): ActionResult {
    const h = this.snap.agent.harness;
    if (!h) return { ok: false, error: "No provider is picked yet." };
    const cat = fakeModels(h);
    const mode = this.snap.activeMode ?? "chat";
    const slotArg = typeof a.slot === "string" ? a.slot : "current";
    const slot = (slotArg === "current" ? mode : slotArg) as "chat" | "build" | "strong";
    const q = typeof a.model === "string" ? a.model.toLowerCase() : null;
    const prev = this.modelPrefs[slot] ?? (slot === "build" ? this.modelPrefs.chat : undefined);
    const m = q ? cat.find((x) => x.id === q || x.displayName.toLowerCase().includes(q)) : cat.find((x) => x.id === (prev?.model ?? this.snap.agent.model));
    if (!m) return { ok: false, error: `No model matches "${String(a.model)}"` };
    const effort = typeof a.effort === "string" && m.efforts.includes(a.effort) ? a.effort : prev?.model === m.id ? prev.effort ?? null : null;
    this.modelPrefs[slot] = { model: m.id, effort };
    this.emitSnap();
    return { ok: true };
  }

  // ---- event plumbing ----
  private emitSnap() {
    this.decorateModels();
    const s = clone(this.snap);
    this.snapCbs.forEach((cb) => cb(s));
  }
  private chatEvt(e: ChatEvent) {
    if (e.op === "append") this.snap.chat.push(clone(e.item));
    else {
      const it = this.snap.chat.find((c) => c.id === e.id);
      if (it) Object.assign(it, e.patch);
    }
    this.chatCbs.forEach((cb) => cb(clone(e)));
  }
  private stamp: { turnId: string; mode: Mode } | null = null;
  private append(item: ChatItem) {
    if (this.stamp) Object.assign(item, this.stamp);
    this.chatEvt({ op: "append", item });
  }
  private update(id: string, patch: Partial<ChatItem>) {
    this.chatEvt({ op: "update", id, patch });
  }
  private notify(text: string) {
    this.notifyCbs.forEach((cb) => cb({ text }));
  }
  private setAgent(status: Snapshot["agent"]["status"]) {
    this.snap.agent.status = status;
    this.emitSnap();
  }
  private pushHistory() {
    this.history.push({ layout: clone(this.snap.layout), theme: clone(this.snap.theme) });
  }
  private refreshViews() {
    const v = this.snap.views;
    if (v["demo/notes"]) v["demo/notes"] = notesView(this.notes);
    if (v["demo/overview"]) v["demo/overview"] = overviewView(this.flags);
    if (v["demo/mic"]) {
      v["demo/mic"] = {
        kind: "spec",
        tree: { type: "icon-button", icon: "mic", label: "Voice input", action: { tool: "toggle_mic" }, active: this.flags.mic },
      };
    }
  }

  // ---- Kernel API ----
  async getSnapshot() {
    return clone(this.snap);
  }
  onSnapshot = async (cb: (s: Snapshot) => void): Promise<Unsub> => (this.snapCbs.add(cb), () => void this.snapCbs.delete(cb));
  onChat = async (cb: (e: ChatEvent) => void): Promise<Unsub> => (this.chatCbs.add(cb), () => void this.chatCbs.delete(cb));
  onComposer = async (cb: (e: ComposerEvent) => void): Promise<Unsub> => (this.compCbs.add(cb), () => void this.compCbs.delete(cb));
  onNotify = async (cb: (e: NotifyEvent) => void): Promise<Unsub> => (this.notifyCbs.add(cb), () => void this.notifyCbs.delete(cb));
  // §21 G1 live channels: in-memory loopback (publish → every subscriber of the topic)
  private chanCbs = new Set<(m: ChannelMessage) => void>();
  private chanSeq = 0;
  private chanRing: Record<string, ChannelMessage[]> = {};
  onChannel = async (cb: (m: ChannelMessage) => void): Promise<Unsub> => (this.chanCbs.add(cb), () => void this.chanCbs.delete(cb));
  async channelSubscribe(_placementId: string, topic: string) {
    return { sub: ++this.chanSeq, topic, replay: [...(this.chanRing[topic] ?? [])] };
  }
  async channelUnsubscribe(_sub: number) {}
  async channelPublish(_placementId: string, message: { topic: string; data?: unknown; b64?: string }) {
    const m: ChannelMessage = { ...message, seq: ++this.chanSeq, ts: Date.now() };
    const ring = (this.chanRing[message.topic] ??= []);
    ring.push(m);
    if (ring.length > 64) ring.shift();
    this.chanCbs.forEach((cb) => cb(m));
    return { ok: true, topic: message.topic };
  }

  async selectHarness(id: string) {
    const h = SHOWN_HARNESSES.find((x) => x.id === id);
    if (!h || !h.available || h.comingSoon) throw `Provider "${id}" is not available`;
    this.snap.agent = { name: h.name, status: "starting", harness: id };
    this.emitSnap();
    await sleep(1000);
    if (this.snap.agent.harness !== id) return;
    this.snap.agent.status = "ready";
    this.emitSnap();
  }

  async harnessLoginSubmit(id: string, kind: "code" | "apikey", _value: string, provider?: string) {
    const h = HARNESSES.find((x) => x.id === id);
    if (!h?.vm) throw `Provider "${id}" signs in outside OpenApp`;
    h.login = undefined;
    h.signedIn = true;
    h.note = kind === "apikey" ? `API key: ${provider ?? "provider"} (in the app VM)` : "Signed in (in the app VM)";
    this.emitSnap();
  }

  async harnessLogin(id: string, method?: string) {
    const h = HARNESSES.find((x) => x.id === id);
    if (!h?.vm) throw `Provider "${id}" signs in outside OpenApp`;
    if (method === "cancel") {
      h.login = { state: "required", methods: h.login?.methods };
      this.emitSnap();
      return;
    }
    h.login = method === "device" ? { state: "device", url: "https://auth.openai.com/codex/device", code: "MOCK-1234" } : { state: "browser", url: "https://auth.openai.com/oauth/authorize?mock", paste: id === "pi" };
    h.note = method === "device" ? "Open the link and enter the code MOCK-1234" : "Finish signing in in your browser…";
    this.emitSnap();
    await sleep(2500);
    h.login = undefined;
    h.signedIn = true;
    h.note = "Signed in with ChatGPT (in the app VM)";
    this.emitSnap();
  }

  async answerQuestion(requestId: string, answers: Record<string, string[]>) {
    const cancelled = Object.keys(answers).length === 0;
    this.update(requestId, cancelled ? { resolved: null } : { resolved: true, answers });
    this.askResolve?.(cancelled ? null : answers);
    this.askResolve = null;
  }

  async submitSecret(requestId: string, value: string | null) {
    // the mock never keeps the value
    this.update(requestId, { resolved: value ? "saved" : null });
  }

  async saveAudio(placementId: string, base64: string, mime: string) {
    console.info("[mock] save_audio", placementId, mime, base64.length, "base64 chars");
    await sleep(900);
    this.compCbs.forEach((cb) => cb({ insert: "transcribed text " }));
    this.notify("Transcribed (mock)");
  }

  async webBounds(placementId: string, rect: WebRect | null) {
    const w = window as unknown as { __webBounds?: Record<string, WebRect | null>; __webLog?: unknown[] };
    (w.__webBounds ??= {})[placementId] = rect;
    (w.__webLog ??= []).push([placementId, rect]);
  }

  async cancelTurn() {
    this.cancelled = true;
    this.askResolve?.(null);
    if (this.permResolve) this.permResolve(null);
  }

  async respondPermission(requestId: string, optionId: string | null) {
    this.update(requestId, { resolved: optionId });
    this.permResolve?.(optionId);
    this.permResolve = null;
  }

  async reportUiError(placementId: string, message: string) {
    console.warn("[mock] report_ui_error", placementId, message);
  }

  async iframeCall(module: string, tool: string, args: Record<string, unknown>): Promise<CallToolResult> {
    await sleep(250);
    if (tool === "fail") throw `tool ${module}/${tool} failed (mock)`;
    return { content: [{ type: "text", text: `${module}.${tool}(${JSON.stringify(args)}) -> pong @ ${new Date().toLocaleTimeString()}` }] };
  }

  async uiAction(placementId: string, action: Action, input: Record<string, unknown> = {}): Promise<ActionResult> {
    console.info("[mock] ui_action", placementId, action, input);
    if ("host" in action) {
      const a = action as Record<string, unknown>;
      switch (action.host) {
        case "composer.insert": this.compCbs.forEach((cb) => cb({ insert: String(a.text ?? "") })); break;
        case "prompt": void this.sendPrompt(String(a.text ?? "")); break;
        case "open_url": window.open(String(a.url ?? ""), "_blank", "noopener"); break;
        case "view.open": {
          const view = String(a.view ?? "");
          if (!this.snap.views[view]) return { ok: false, error: `unknown view ${view}` };
          const as = (["sheet", "modal", "inline", "tab"].includes(String(a.as)) ? String(a.as) : "sheet") as "sheet";
          (this.snap.presented ??= []).push({ id: "pr_" + Math.random().toString(16).slice(2, 8), view, as, title: a.title ? String(a.title) : undefined, width: typeof a.width === "number" ? a.width : undefined });
          this.emitSnap();
          break;
        }
        case "view.close": this.snap.presented?.pop(); this.emitSnap(); break;
        case "turn.undo": {
          const h = this.history.pop();
          if (h) { this.snap.layout = h.layout; this.snap.theme = h.theme; }
          this.append({ id: nid("s"), kind: "system", text: "Undid the build turn." });
          this.emitSnap();
          break;
        }
        case "mode.set": this.setMode(a.mode === "build" ? "build" : "chat"); break;
        case "model.set": return this.setModel(a);
        case "model.clear": { const sl = typeof a.slot === "string" && a.slot !== "current" ? a.slot : (this.snap.activeMode ?? "chat"); delete this.modelPrefs[sl as "chat" | "build" | "strong"]; this.emitSnap(); break; }
        case "model.policy": this.modelPrefs.policy = a.policy === "auto" ? "auto" : "ask"; this.emitSnap(); break;
        case "models.refresh": await sleep(400); this.emitSnap(); break;
        case "session.new": this.sessionNew(); break;
        case "session.open": this.sessionOpen(String(a.id ?? "")); break;
        case "session.rename": this.sessionRename(String(a.id ?? ""), String(a.title ?? "")); break;
        case "harness.select": try { await this.selectHarness(String(a.id ?? "")); } catch (e) { return { ok: false, error: String(e) }; } break;
        case "harness.refresh": break;
        case "harness.login": void this.harnessLogin(String(a.id ?? ""), typeof a.method === "string" ? a.method : undefined); break;
        case "grant.revoke": this.snap.grants = (this.snap.grants ?? []).filter((g) => g.id !== a.id); this.emitSnap(); break;
        case "notify": this.notify(String(a.text ?? "Notification")); break;
        default: return { ok: false, error: `unknown host action: ${String(a.host)}` };
      }
      return { ok: true };
    }
    const args = { ...(action.args ?? {}), ...input };
    switch (action.tool) {
      case "toggle_mic":
        this.flags.mic = !this.flags.mic;
        this.notify(this.flags.mic ? "Listening… (mock)" : "Stopped listening");
        break;
      case "toggle_flag": this.flags.toggled = Boolean(args.value); break;
      case "add_note":
        if (typeof args.value === "string" && args.value.trim()) this.notes.unshift(args.value.trim());
        break;
      case "ping":
        await sleep(1200);
        this.flags.progress = Math.min(1, this.flags.progress + 0.25);
        this.notify("pong from the mock module server");
        break;
      case "fail": return { ok: false, error: "The tool exploded (mock failure)." };
      default: return { ok: false, error: `no such tool: ${action.tool}` };
    }
    this.refreshViews();
    this.emitSnap();
    return { ok: true };
  }

  async sendPrompt(raw: string): Promise<void> {
    const text = raw.trim();
    if (!text) return;
    if (text === "/reset") {
      const keep = { sessions: this.snap.sessions, activeSession: this.snap.activeSession, agent: this.snap.agent };
      this.snap = { ...initial(), ...keep, chat: [] };
      this.history = [];
      this.flags = { toggled: false, progress: 0.35, mic: false };
      this.emitSnap();
      return;
    }
    const mm = /^\/(build|chat)(?:\s+([\s\S]*))?$/.exec(text);
    if (mm) {
      this.setMode(mm[1] as Mode);
      if (mm[2]?.trim()) return this.sendPrompt(mm[2]);
      this.append({ id: nid("s"), kind: "system", text: `Switched to ${mm[1]} mode.` });
      return;
    }
    const mo = /^\/model(?:\s+(strong|build|chat))?(?:\s+(.+))?$/.exec(text);
    if (mo) {
      this.append({ id: nid("u"), kind: "user", text });
      if (!mo[2]) {
        this.append({ id: nid("s"), kind: "system", text: `Model: ${this.snap.agent.modelName ?? "default"}. Available (fake): ${fakeModels(this.snap.agent.harness ?? "mock").map((m) => m.displayName).join(", ")}.` });
        return;
      }
      const words = mo[2].trim().split(/\s+/);
      const effort = ["low", "medium", "high", "xhigh", "max"].includes(words[words.length - 1]!.toLowerCase()) ? words.pop()!.toLowerCase() : undefined;
      const r = this.setModel({ model: words.join(" "), effort, slot: mo[1] ?? "current" });
      this.append({ id: nid("s"), kind: "system", text: r.ok ? `Model: ${this.snap.agent.modelName}${this.snap.agent.effort ? ` · ${this.snap.agent.effort}` : ""}.` : r.error ?? "No match." });
      return;
    }
    if (text === "/provider") {
      this.snap.agent = { name: "No provider", status: "unselected", harness: null };
      this.append({ id: nid("s"), kind: "system", text: "Provider cleared. Pick a provider to continue." });
      this.emitSnap();
      return;
    }
    if (text === "/fail") {
      this.snap.agent.status = "error";
      this.snap.agent.error = "Codex: not signed in. Run `codex login` and try again.";
      this.emitSnap();
      return;
    }
    if (this.snap.agent.status === "unselected") return;
    if (text === "/undo") {
      this.append({ id: nid("u"), kind: "user", text });
      const h = this.history.pop();
      if (h) {
        this.snap.layout = h.layout;
        this.snap.theme = h.theme;
        this.emitSnap();
        this.append({ id: nid("s"), kind: "system", text: "Reverted last layout/theme change." });
      } else this.append({ id: nid("s"), kind: "system", text: "Nothing to undo." });
      return;
    }
    this.stamp = this.snap.activeMode === "build" ? { turnId: nid("turn"), mode: "build" } : null;
    this.append({ id: nid("u"), kind: "user", text });
    this.touchSession(text);
    this.cancelled = false;
    const turn = ++this.turn;
    this.setAgent("working");
    try {
      await sleep(350);
      const lower = text.toLowerCase();
      if (this.snap.activeMode === "build" && lower.includes("scheduler")) await this.runBuildScheduler();
      else if (lower === "demo") await this.runDemo();
      else if (lower === "sheet" || lower === "modal" || lower === "inline") await this.runPresent(lower);
      else if (lower === "html") await this.runHtml();
      else if (lower === "theme") await this.runTheme();
      else if (lower === "permission") await this.runPermission();
      else if (lower === "mic") await this.runMic();
      else if (lower === "ask") await this.runAsk();
      else if (lower === "web") await this.runWeb();
      else if (lower === "rail") await this.runRail();
      else if (lower === "broken") await this.runBroken();
      else if (lower === "error") this.append({ id: nid("e"), kind: "error", text: "Mock error: the agent crashed politely." });
      else await this.reply(
        `You said: **${text}**\n\nThis is the mock agent. In the real app Claude Code would build it. Try:\n\n- \`demo\` — regions, tabs, a composer button, a card\n- \`html\` — a sandboxed html view\n- \`theme\` — cycle the theme\n- \`permission\` — a permission card\n- \`rail\` — a sessions rail bound to host data\n- \`mic\` — a record button + failed command chip\n- \`ask\` — an agent question card\n- \`web\` — a web placement\n- \`sheet\` / \`modal\` / \`inline\` — presentations\n- \`/build scheduler\` — a long, quiet build turn\n- \`/build\`, \`/chat\` — mode\n- \`broken\` — a bad spec node\n\n\`\`\`ts\nconst answer: number = 42;\n\`\`\`\n[Docs](https://example.com)`,
      );
    } finally {
      this.stamp = null;
      if (turn === this.turn) this.setAgent("ready");
    }
  }

  // ---- mode (§12.1) ----
  private setMode(mode: Mode) {
    this.snap.activeMode = mode;
    const m = this.active();
    if (m) m.mode = mode;
    this.emitSnap();
  }

  // ---- sessions (§10) ----
  private active(): SessionMeta | undefined {
    return this.snap.sessions?.find((x) => x.id === this.snap.activeSession);
  }
  private touchSession(firstPrompt: string) {
    const m = this.active();
    if (!m) return;
    if (m.title === "New chat") m.title = firstPrompt.slice(0, 48);
    m.updated = Date.now();
    this.snap.sessions!.sort((x, y) => Number(y.updated) - Number(x.updated));
    this.emitSnap();
  }
  private sessionNew() {
    this.chats[this.snap.activeSession!] = this.snap.chat;
    const now = Date.now();
    const id = "s_" + Math.random().toString(16).slice(2, 10);
    this.snap.sessions!.unshift({ id, title: "New chat", created: now, updated: now });
    this.snap.activeSession = id;
    this.chats[id] = this.snap.chat = [];
    this.emitSnap();
  }
  private sessionOpen(id: string) {
    if (id === this.snap.activeSession || !this.snap.sessions?.some((x) => x.id === id)) return;
    this.chats[this.snap.activeSession!] = this.snap.chat;
    this.snap.activeSession = id;
    this.snap.chat = this.chats[id] ?? [];
    this.emitSnap();
  }
  private sessionRename(id: string, title: string) {
    const m = this.snap.sessions?.find((x) => x.id === id);
    if (m && title.trim()) m.title = title.trim().slice(0, 48);
    this.emitSnap();
  }

  // ---- fake agent behaviours ----
  private async toolCmd(command: string, lines: string[], exitCode: number, ms = 250) {
    const id = nid("t");
    this.append({ id, kind: "tool", title: "Run command", command, cwd: "/Users/you/.openapp/workspace/modules/voice", status: "in_progress", output: "" });
    let out = "";
    for (const l of lines) {
      await sleep(ms);
      out += l + "\n";
      this.update(id, { output: out });
    }
    this.update(id, { status: exitCode === 0 ? "completed" : "failed", exitCode });
  }

  private async runMic() {
    await this.toolCmd("npm install @modelcontextprotocol/sdk", ["added 12 packages in 2s"], 0);
    await this.toolCmd("ffmpeg -i data/recordings/last.webm -ar 16000 out.wav", ["ffmpeg version 7.1", "last.webm: No such file or directory", "Error opening input file last.webm."], 1);
    this.pushHistory();
    const pid = "p_" + Math.random().toString(16).slice(2, 10);
    this.snap.layout.placements.push({ id: pid, module: "voice", view: "rec", anchor: "composer.actions" });
    this.snap.views["voice/rec"] = {
      kind: "spec",
      tree: { type: "icon-button", icon: "mic", label: "Dictate", action: { host: "audio.record", tool: "transcribe", arg: "path" } },
    };
    this.emitSnap();
    await this.reply("Added a **Dictate** button next to Send. Click to record, click again to stop. Escape cancels.");
  }

  private async runPresent(kind: "sheet" | "modal" | "inline") {
    this.snap.views["demo/tools"] = {
      kind: "spec",
      tree: { type: "stack", gap: 12, padding: 16, children: [
        { type: "text", value: `This is a **${kind}** presentation of a spec view.`, variant: "body" },
        { type: "list", items: [{ id: "1", title: "Item one", icon: "wrench" }, { id: "2", title: "Item two", icon: "wrench" }] },
        { type: "button", label: "Close", variant: "secondary", action: { host: "view.close" } },
      ] },
    };
    this.snap.views["demo/opener"] = {
      kind: "spec",
      tree: { type: "stack", direction: "row", gap: 8, children: [
        { type: "button", label: "Sheet", icon: "panel-right", action: { host: "view.open", view: "demo/tools", as: "sheet", title: "Tools" } },
        { type: "button", label: "Modal", icon: "square", action: { host: "view.open", view: "demo/tools", as: "modal", title: "Confirm" } },
        { type: "button", label: "Inline", icon: "message-square", action: { host: "view.open", view: "demo/tools", as: "inline", title: "Answer" } },
      ] },
    };
    if (!this.snap.layout.placements.some((p) => p.module === "demo" && p.view === "opener")) {
      this.snap.layout.placements.push({ id: "p_" + Math.random().toString(16).slice(2, 10), module: "demo", view: "opener", anchor: "main", order: 0 });
    }
    const firstUser = this.snap.chat.find((c) => c.kind === "user");
    (this.snap.presented ??= []).push({ id: "pr_" + Math.random().toString(16).slice(2, 8), view: "demo/tools", as: kind, after: kind === "inline" ? firstUser?.id : undefined, title: kind === "sheet" ? "Tools" : kind === "modal" ? "Confirm" : "Answer" });
    this.emitSnap();
    await this.reply(`Opened a **${kind}**. The pinned strip above has buttons for all three.`);
  }

  private async runBuildScheduler() {
    const steps: [string, number][] = [
      ["openapp_describe", 250], ["Read modules/demo/openapp.json", 200], ["Create modules/scheduler/openapp.json", 250],
      ["Write modules/scheduler/views/scheduler.json", 250], ["Edit views/scheduler.json", 250],
    ];
    for (const [t, ms] of steps) await this.tool(t, ms);
    await this.toolCmd("npm install cron-parser", ["added 3 packages in 1s"], 0, 400);
    await this.tool("Write modules/scheduler/server.ts", 300);
    await this.tool("openapp_module_load", 400);
    // a question, inline in the card
    const id = nid("q");
    this.append({ id, kind: "question", title: "Question", questions: [{ id: "tz", header: "Time zone", question: "Which time zone should the scheduler use?", options: [{ label: "Local", description: "Use this Mac's time zone." }, { label: "UTC", description: "Same everywhere." }], isOther: false }] });
    const ans = await new Promise<Record<string, string[]> | null>((r) => (this.askResolve = r));
    this.pushHistory();
    const l = this.snap.layout;
    this.snap.views["scheduler/panel"] = { kind: "spec", tree: { type: "stack", gap: 10, padding: 16, children: [
      { type: "text", value: "Scheduler", variant: "heading" },
      { type: "text", value: `Time zone: ${ans?.tz?.[0] ?? "Local"}`, variant: "muted" },
      { type: "list", items: [{ id: "1", title: "Daily standup", subtitle: "09:30", icon: "calendar" }], empty: "No jobs." },
    ] } };
    this.snap.views["scheduler/open"] = { kind: "spec", tree: { type: "icon-button", icon: "calendar", label: "Scheduler", action: { host: "view.open", view: "scheduler/panel", as: "sheet", title: "Scheduler" } } };
    l.placements.push({ id: "p_" + Math.random().toString(16).slice(2, 10), module: "scheduler", view: "open", anchor: "composer.actions" });
    this.emitSnap();
    await this.tool("openapp_layout", 300);
    await this.tool("openapp_snapshot", 250);
    await this.reply("Added a **Scheduler** button next to Send. It opens the scheduler in a side panel. Undo removes it.");
  }

  private async runAsk() {
    const id = nid("q");
    const questions: Question[] = [
      {
        id: "stack", header: "Stack", question: "Which backend should the notes module use?",
        options: [
          { label: "Node (TypeScript)", description: "Runs .ts directly, no build step." },
          { label: "Python", description: "Good if you want to use local ML libraries." },
        ],
        isOther: true,
      },
      { id: "key", header: "API key", question: "Paste the API key for the sync service.", options: null, isSecret: true },
    ];
    this.append({ id, kind: "question", title: "The agent has questions", questions });
    const ans = await new Promise<Record<string, string[]> | null>((r) => (this.askResolve = r));
    await this.reply(ans ? `Got it: stack = **${ans.stack?.[0] ?? "?"}**.` : "Okay, I will skip that.");
  }

  private async runWeb() {
    this.pushHistory();
    const pid = "p_" + Math.random().toString(16).slice(2, 10);
    this.snap.layout.placements.push({ id: pid, module: "gemini", view: "site", anchor: "main", order: 3 });
    this.snap.views["gemini/site"] = { kind: "web", url: "https://gemini.google.com/app", profile: "google" };
    (this.snap.web ??= {})[pid] = { url: "https://gemini.google.com/app", title: "Gemini", loading: false };
    this.emitSnap();
    await this.tool("openapp_layout");
    await this.reply("Placed a **web surface** (mock: placeholder only) in the main column.");
  }

  private async runRail() {
    await this.tool("Write modules/rail/views/sessions.json", 300);
    this.pushHistory();
    const l = this.snap.layout;
    if (!l.regions.some((r) => r.id === "rail")) l.regions.push({ id: "rail", edge: "left", size: 250, mode: "stack" });
    l.placements.push({ id: "p_" + Math.random().toString(16).slice(2, 10), module: "rail", view: "sessions", anchor: "region:rail", order: 0 });
    this.snap.views["rail/sessions"] = {
      kind: "spec",
      tree: {
        type: "stack", gap: 10, padding: 12, children: [
          { type: "text", value: { $host: "session.active.title" }, variant: "heading" },
          { type: "button", label: "New chat", icon: "plus", variant: "secondary", action: { host: "session.new" } },
          { type: "list", items: { $host: "sessions" }, empty: "No chats yet." },
          { type: "list", items: { $host: "harnesses" } },
          { type: "text", value: { $host: "agent.status" }, variant: "small" },
        ],
      },
    };
    this.emitSnap();
    await this.reply("Added a **sessions rail** on the left, bound to host data.");
  }

  private async reply(full: string) {
    const id = nid("a");
    this.append({ id, kind: "agent", text: "" });
    const parts = full.match(/\S+\s*/g) ?? [full];
    let acc = "";
    for (const p of parts) {
      if (this.cancelled) { acc += " (stopped)"; this.update(id, { text: acc }); return; }
      acc += p;
      this.update(id, { text: acc });
      await sleep(28);
    }
  }

  private async tool(title: string, ms = 500) {
    const id = nid("t");
    this.append({ id, kind: "tool", title, status: "pending" });
    await sleep(120);
    this.update(id, { status: "in_progress" });
    await sleep(ms);
    this.update(id, { status: "completed" });
  }

  private async thought(text: string) {
    const id = nid("th");
    this.append({ id, kind: "thought", text });
    await sleep(300);
  }

  private async runDemo() {
    await this.thought("The user wants a feature. I should check the layout, write a module, then place it.");
    await this.tool("openapp_describe");
    await this.tool("Write modules/demo/openapp.json", 300);
    await this.tool("openapp_module_load");
    this.pushHistory();
    const l = this.snap.layout;
    if (!l.regions.find((r) => r.id === "left")) l.regions.push({ id: "left", edge: "left", size: 300, mode: "tabs", title: "Workspace" });
    const add = (view: string, anchor: string, label?: string, order = 0) =>
      l.placements.push({ id: "p_" + Math.random().toString(16).slice(2, 10), module: "demo", view, anchor, label, order });
    if (!l.placements.length) {
      add("notes", "region:left", "Notes", 0);
      add("panel", "region:left", "Panel", 1);
      add("mic", "composer.actions");
      add("overview", "main");
      add("extra", "slot:extras");
    }
    this.snap.modules = [{ id: "demo", name: "Demo module", status: "running" }];
    this.snap.views["demo/notes"] = notesView(this.notes);
    this.snap.views["demo/overview"] = overviewView(this.flags);
    this.snap.views["demo/mic"] = { kind: "spec", tree: { type: "icon-button", icon: "mic", label: "Voice input", action: { tool: "toggle_mic" } } };
    this.snap.views["demo/extra"] = {
      kind: "spec",
      tree: { type: "stack", direction: "row", gap: 8, align: "center", children: [{ type: "badge", label: "slot", tone: "accent" }, { type: "text", value: "I live inside a `slot` node.", variant: "small" }] },
    };
    this.snap.views["demo/panel"] = clone(MOCK_HTML);
    this.refreshViews();
    this.emitSnap();
    await this.tool("openapp_layout");
    await this.reply("Done. I added a **left rail** with tabs (Notes, Panel), a mic button next to Send, and a card in the main column.");
  }

  private async runHtml() {
    this.pushHistory();
    this.snap.views["demo/panel"] = { kind: "html", url: "/mock/panel.html", rev: ((this.snap.views["demo/panel"] as { rev?: number } | undefined)?.rev ?? 0) + 1 };
    if (!this.snap.layout.placements.some((p) => p.module === "demo" && p.view === "panel" && p.anchor === "main")) {
      this.snap.layout.placements.push({ id: "p_" + Math.random().toString(16).slice(2, 10), module: "demo", view: "panel", anchor: "main", order: 5 });
    }
    this.emitSnap();
    await this.tool("openapp_layout");
    await this.reply("Placed an html view in the main column.");
  }

  private async runTheme() {
    this.pushHistory();
    const p = PALETTES[this.paletteIdx++ % PALETTES.length]!;
    this.snap.theme = { ...DEFAULT_THEME, ...p.tokens };
    this.emitSnap();
    await this.tool(`openapp_theme (${p.name})`, 200);
    await this.reply(`Applied the **${p.name}** theme.`);
  }

  private async runBroken() {
    this.snap.layout.placements.push({ id: "p_" + Math.random().toString(16).slice(2, 10), module: "demo", view: "broken", anchor: "main", order: 9 });
    this.snap.views["demo/broken"] = {
      kind: "spec",
      tree: { type: "card", title: "Broken view", children: [{ type: "sparkle", value: 1 }, { type: "text", value: "…but the rest still renders." }] },
    };
    this.emitSnap();
    await this.reply("Added a view containing an unknown node type.");
  }

  private async runPermission() {
    const id = nid("perm");
    this.append({
      id,
      kind: "permission",
      title: "Run: npm install @modelcontextprotocol/sdk zod",
      details: { summary: "Adds a host module that can run shell commands.", frameworks: ["AppKit", "Foundation"], calls: ["Process()", "NSAppleScript", "URLSession"] },
      diff: "--- a/Host.swift\n+++ b/Host.swift\n@@ -1,3 +1,4 @@\n import Foundation\n-let x = 1\n+let x = 2\n+Process().run()\n",
      options: [
        { optionId: "allow", name: "Allow once", kind: "allow_once" },
        { optionId: "always", name: "Always allow", kind: "allow_always" },
        { optionId: "deny", name: "Reject", kind: "reject_once" },
      ],
    });
    const choice = await new Promise<string | null>((r) => (this.permResolve = r));
    await this.reply(choice && choice !== "deny" ? "Thanks — installing." : "Okay, I will not run that.");
  }
}
