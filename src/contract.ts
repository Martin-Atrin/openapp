// OpenApp MVP contract v0.1 — shell-side types (spec/MVP-CONTRACT.md §4, §5, §8).

export type Edge = "left" | "right" | "top" | "bottom";

export interface Region {
  id: string;
  edge: Edge;
  /** px: width for left/right, height for top/bottom */
  size: number;
  mode: "stack" | "tabs";
  title?: string;
}

export interface Placement {
  id: string;
  module: string;
  view: string;
  /** composer.actions | composer.above | main | region:<id> | slot:<id> */
  anchor: string;
  label?: string;
  order?: number;
}

export interface Layout {
  regions: Region[];
  placements: Placement[];
}

export interface ModuleInfo {
  id: string;
  name: string;
  /** §21 G9: idle = views shown, server not started yet (lazy activation) or stopped after idle */
  status: "stopped" | "running" | "idle" | "error";
  error?: string;
}

export type ViewDef =
  | { kind: "spec"; tree: unknown }
  | { kind: "html"; url: string; rev: number }
  | { kind: "web"; url: string; profile?: string }
  | { kind: "error"; error: string };

/** §13: shell-reported rect of a web placement, CSS px relative to the viewport. */
export interface WebRect {
  x: number;
  y: number;
  width: number;
  height: number;
}
export interface WebState {
  url: string;
  title: string;
  loading: boolean;
}
export type Mode = "chat" | "build";

/** §19.1 presentation (kernel chrome) */
export interface Presented {
  id: string;
  /** "<module>/<view>" */
  view: string;
  as: "sheet" | "modal" | "inline" | "tab";
  title?: string;
  width?: number;
  /** inline: chat item id to insert after (fallback: end of thread) */
  after?: string;
}

export type AgentStatus = "unselected" | "starting" | "ready" | "working" | "error";

export interface HarnessInfo {
  id: string;
  name: string;
  adapter: "codex-app-server" | "acp" | "pi-rpc";
  available: boolean;
  /** null = unknown */
  signedIn: boolean | null;
  note?: string;
  /** §17: runs inside the app VM */
  vm?: boolean;
  /** Release gating (§11): not usable in this build; show dimmed with a "Coming soon" badge */
  comingSoon?: boolean;
  /** §17: sign-in flow of a VM harness (absent when signed in or unknown) */
  login?: HarnessLogin;
  /** §17: stored logins of a VM harness (Pi) and the sign-in choices, for the account card */
  accounts?: HarnessAccounts;
}

/** §17 the account card of a VM harness: sign in again, add a provider, replace an API key, sign out. */
export interface HarnessAccounts {
  /** never holds a credential value */
  logins: { provider: string; type: "oauth" | "api_key" | string; label: string; active: boolean }[];
  methods: HarnessLoginMethod[];
}

/** §17 sign-in flow state of a VM harness. */
export interface HarnessLogin {
  state: "required" | "starting" | "browser" | "device" | "failed";
  url?: string;
  code?: string;
  message?: string;
  /** Pi: sign-in choices (state required/failed); absent = the Codex buttons */
  methods?: HarnessLoginMethod[];
  /** the browser step also accepts a pasted redirect URL (`harnessLoginSubmit` kind "code") */
  paste?: boolean;
  method?: string | null;
}

/** One sign-in choice of a VM harness (Pi): OAuth (`chatgpt`, `claude`) or `apikey` with providers. */
export interface HarnessLoginMethod {
  id: string;
  label: string;
  note?: string;
  providers?: { id: string; label: string }[];
}

/** §17 host data `grants`: what the user allowed. */
export interface Grant {
  id: string;
  /** §21 G4/G5 adds clipboard.read, shortcut.global, camera (per module) and folder (granted folders) */
  kind: "network" | "microphone" | "web.control" | "clipboard.read" | "shortcut.global" | "camera" | "folder";
  target: string;
  allowed: boolean;
  scope?: string | null;
  ts?: string | null;
}

export interface AgentInfo {
  name: string;
  status: AgentStatus;
  error?: string;
  /** id of the picked harness, null when none */
  harness?: string | null;
  /** §23 model of the active session's next turn (harness id); absent = the agent's own default */
  model?: string;
  /** §23 effort in the harness's vocabulary */
  effort?: string | null;
  modelName?: string;
  tier?: "default" | "strong";
}

/** §23 a model the signed-in account offers (discovered at runtime, never hardcoded). */
export interface ModelInfo {
  id: string;
  displayName: string;
  description?: string;
  /** lowest first, in the harness's vocabulary (e.g. low … xhigh … max) */
  efforts: string[];
  defaultEffort?: string | null;
  hidden?: boolean;
  isDefault?: boolean;
  harness: string;
}

export interface ModelChoice {
  model: string;
  effort?: string | null;
}

/** §23 choices of the selected harness. */
export interface ModelPrefs {
  harness: string;
  chat?: ModelChoice | null;
  build?: ModelChoice | null;
  strong?: ModelChoice | null;
  policy: "ask" | "auto";
  source?: string | null;
  fetchedAt?: string | null;
  error?: string | null;
  /** reason of the active session's escalation to the strong model */
  escalated?: string | null;
  current?: { model: string; effort?: string | null; tier: "default" | "strong"; label: string } | null;
}

export type ThemeValue = string | number;

/** Timestamps may be ms or s since epoch, or an ISO string; the shell normalises. */
export type Timestamp = number | string;

export interface SessionMeta {
  id: string;
  title: string;
  created: Timestamp;
  updated: Timestamp;
  mode?: Mode;
}

export interface Snapshot {
  layout: Layout;
  theme: Record<string, ThemeValue>;
  modules: ModuleInfo[];
  /** key: "<module>/<view>" */
  views: Record<string, ViewDef>;
  agent: AgentInfo;
  chat: ChatItem[];
  /** §10: newest `updated` first */
  sessions?: SessionMeta[];
  activeSession?: string;
  /** §11 */
  harnesses?: HarnessInfo[];
  /** §12.1 */
  activeMode?: Mode;
  /** §13: key = placement id */
  web?: Record<string, WebState>;
  /** §19.1 */
  presented?: Presented[];
  /** §17 */
  grants?: Grant[];
  /** §20.1 host data `secrets` (metadata only, never values) */
  secrets?: SecretMeta[];
  /** §20.3 guide strip above a web view */
  guide?: Guide | null;
  /** §21 host data of registry primitives: `{ "$host": name }` → hostData[name] */
  hostData?: Record<string, unknown>;
  /** §23 discovered models of every harness seen (filter by agent.harness) */
  models?: ModelInfo[];
  /** §23 */
  modelPrefs?: ModelPrefs | null;
}

/** §21 G1 live channel message (`oa://channel`, channel_subscribe replay): JSON `data` or base64 `b64`. */
export interface ChannelMessage {
  topic: string;
  seq: number;
  ts: number;
  data?: unknown;
  b64?: string;
  /** live messages: the subscription they belong to */
  sub?: number;
}

/** §20.1 secret metadata (the value stays in the Keychain). */
export interface SecretMeta {
  name: string;
  kind: "token" | "password" | "oauth-client" | "oauth-token";
  label?: string;
  provider?: string | null;
  scopes?: string[];
  hosts?: string[];
  used_by?: string[];
  created?: string;
  updated?: string;
  expires_at?: number | null;
}

/** §20.3 guide strip. */
export interface GuideStep {
  title: string;
  detail?: string;
}
export interface Guide {
  /** placement id the strip belongs to (absent = the top presented web view) */
  placement?: string | null;
  steps: GuideStep[];
  current: number;
  done?: number[];
}

export type ChatKind = "user" | "agent" | "thought" | "tool" | "permission" | "system" | "error" | "question" | "secret";

export interface PermissionOption {
  optionId: string;
  name: string;
  kind: string; // allow_once | allow_always | reject_once | reject_always
}

export interface QuestionOption {
  label: string;
  description?: string;
}
export interface Question {
  id: string;
  header?: string;
  question: string;
  options: QuestionOption[] | null;
  isOther?: boolean;
  isSecret?: boolean;
}

export interface ChatItem {
  id: string;
  kind: ChatKind;
  text?: string;
  title?: string;
  status?: string; // tool: pending | in_progress | completed | failed
  options?: PermissionOption[];
  /** permission: chosen optionId; question: answers map or true; null = cancelled; undefined = open */
  resolved?: string | boolean | Record<string, string[]> | null;
  /** §19.2: items of one build-mode turn share turnId and carry mode */
  turnId?: string;
  mode?: Mode;
  /** tool chips for commands (§15.3) */
  command?: string;
  cwd?: string;
  output?: string;
  exitCode?: number | null;
  /** permission: host-module consent (unified diff + analysis) */
  diff?: string;
  details?: { summary?: string; frameworks?: string[]; calls?: string[]; [k: string]: unknown };
  /** question */
  questions?: Question[];
  answers?: Record<string, string[]>;
  /** §20.1 secret card: title = label, text = help; resolved "saved" | null */
  name?: string;
  secretKind?: "token" | "password" | "oauth-client";
  requestedBy?: string;
  hosts?: string[];
  /** secret / system (OAuth): a link to open; system: a device code to show */
  url?: string | null;
  linkLabel?: string;
  code?: string | null;
  /** §23 system line: the session switched to the strong model (shown in the build card) */
  modelSwitch?: boolean;
}

export type ChatEvent =
  | { op: "append"; item: ChatItem }
  | { op: "update"; id: string; patch: Partial<ChatItem> };

// ---- Actions (§4.1) ----
export type ToolAction = { tool: string; args?: Record<string, unknown> };
export type HostAction =
  | { host: "audio.record"; tool: string; arg: string }
  | { host: "mode.set"; mode: Mode }
  | { host: "model.set"; model?: string; effort?: string; slot?: "current" | "chat" | "build" | "strong" }
  | { host: "model.clear"; slot?: "current" | "chat" | "build" | "strong" }
  | { host: "model.policy"; policy: "ask" | "auto" }
  | { host: "models.refresh"; includeHidden?: boolean }
  | { host: "view.open"; view: string; as?: "sheet" | "modal" | "inline" | "tab"; title?: string; width?: number; region?: string }
  | { host: "view.close"; view?: string }
  | { host: "turn.undo"; turnId?: string }
  | { host: "web.navigate"; placement: string; url: string }
  | { host: "web.back" | "web.forward" | "web.reload"; placement: string }
  | { host: "harness.select"; id: string }
  | { host: "harness.refresh" }
  | { host: "harness.login"; id: string; method?: string }
  | { host: "grant.revoke"; id: string }
  | { host: "secret.delete"; name: string }
  | { host: "guide.done"; step?: number }
  | { host: "guide.clear" }
  | { host: "session.new" }
  | { host: "session.open"; id: string }
  | { host: "session.rename"; id: string; title: string }
  | { host: "composer.insert"; text: string }
  | { host: "prompt"; text: string }
  | { host: "open_url"; url: string }
  | { host: "notify"; text?: string; [k: string]: unknown };
export type Action = ToolAction | HostAction;

export interface ActionResult {
  ok: boolean;
  error?: string;
  /** host.call / registry actions: the action's result */
  result?: unknown;
}

// ---- Spec nodes (§4.1) — documentation of the catalog; the renderer reads defensively. ----
export type Align = "start" | "center" | "end" | "stretch";
export type Justify = "start" | "center" | "end" | "between";
export type Tone = "neutral" | "accent" | "success" | "warning" | "danger";

export interface SpecItem {
  id: string;
  title: string;
  subtitle?: string;
  icon?: string;
  active?: boolean;
  action?: Action;
}

export type KnownSpecNode =
  | { type: "stack"; direction?: "column" | "row"; gap?: number; align?: Align; justify?: Justify; padding?: number; children?: SpecNode[] }
  | { type: "text"; value: string; variant?: "body" | "title" | "heading" | "muted" | "mono" | "small" }
  | { type: "button"; label: string; icon?: string; variant?: "primary" | "secondary" | "ghost" | "danger"; action: Action }
  | { type: "icon-button"; icon: string; label: string; action: Action; active?: boolean }
  | { type: "input"; name: string; placeholder?: string; value?: string; submit?: Action }
  | { type: "toggle"; label: string; value: boolean; action: Action }
  | { type: "list"; items: SpecItem[]; empty?: string }
  | { type: "card"; title?: string; children?: SpecNode[] }
  | { type: "badge"; label: string; tone?: Tone }
  | { type: "progress"; value: number }
  | { type: "image"; src: string; alt?: string; height?: number }
  | { type: "divider" }
  | { type: "slot"; id: string };

export type SpecNode = KnownSpecNode | { type: string; [k: string]: unknown };

// ---- Misc event payloads ----
export interface ComposerEvent {
  insert: string;
}
export interface NotifyEvent {
  text: string;
}

/** MCP CallToolResult (loosely typed). */
export interface CallToolResult {
  content?: { type: string; text?: string; [k: string]: unknown }[];
  structuredContent?: unknown;
  isError?: boolean;
  [k: string]: unknown;
}

/** §10 host data: any spec prop may be `{ "$host": "<source>" }`. */
export type HostSource = "sessions" | "harnesses" | "grants" | "secrets" | "session.active.title" | "agent.name" | "agent.status";
