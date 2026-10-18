// Typed wrappers over the Tauri IPC surface (contract §8) + a browser dev mock.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  Action,
  ActionResult,
  CallToolResult,
  ChannelMessage,
  ChatEvent,
  ComposerEvent,
  NotifyEvent,
  Snapshot,
  WebRect,
} from "./contract";

export type Unsub = () => void;

export interface Kernel {
  readonly isMock: boolean;
  getSnapshot(): Promise<Snapshot>;
  sendPrompt(text: string): Promise<void>;
  cancelTurn(): Promise<void>;
  selectHarness(id: string): Promise<void>;
  /** §17: start the sign-in of a VM harness (Codex: browser, or `device` code; Pi: `chatgpt` | `claude` | `cancel`). */
  harnessLogin(id: string, method?: string): Promise<void>;
  /** §17: input for an open sign-in: `code` = pasted redirect URL, `apikey` = provider API key. Never log or keep it. */
  harnessLoginSubmit(id: string, kind: "code" | "apikey", value: string, provider?: string): Promise<void>;
  answerQuestion(requestId: string, answers: Record<string, string[]>): Promise<void>;
  /** §20.1: the value goes straight to the Keychain; null cancels. Never log or keep it. */
  submitSecret(requestId: string, value: string | null): Promise<void>;
  webBounds(placementId: string, rect: WebRect | null): Promise<void>;
  /** §15.1; tool/arg are extras so the kernel can route even if it cannot infer them from the view */
  saveAudio(placementId: string, base64: string, mime: string, action?: { tool?: string; arg?: string; transcribe?: string; insert?: boolean; locale?: string }): Promise<void>;
  respondPermission(requestId: string, optionId: string | null): Promise<void>;
  uiAction(placementId: string, action: Action, input?: Record<string, unknown>): Promise<ActionResult>;
  iframeCall(module: string, tool: string, args: Record<string, unknown>): Promise<CallToolResult>;
  reportUiError(placementId: string, message: string): Promise<void>;
  onSnapshot(cb: (s: Snapshot) => void): Promise<Unsub>;
  onChat(cb: (e: ChatEvent) => void): Promise<Unsub>;
  onComposer(cb: (e: ComposerEvent) => void): Promise<Unsub>;
  onNotify(cb: (e: NotifyEvent) => void): Promise<Unsub>;
  /** §21 G1: subscribe a placement to a live channel; returns the subscription id and the ring buffer (replay). */
  channelSubscribe(placementId: string, topic: string): Promise<{ sub: number; topic: string; replay: ChannelMessage[] }>;
  channelUnsubscribe(sub: number): Promise<void>;
  /** `message` = { topic, data } | { topic, b64 } (published as the placement's module) */
  channelPublish(placementId: string, message: { topic: string; data?: unknown; b64?: string }): Promise<unknown>;
  onChannel(cb: (m: ChannelMessage) => void): Promise<Unsub>;
}

function makeListener<T>(event: string) {
  return async (cb: (p: T) => void): Promise<Unsub> => {
    const un = await listen<T>(event, (e) => cb(e.payload));
    return un;
  };
}

class TauriKernel implements Kernel {
  readonly isMock = false;
  getSnapshot() {
    return invoke<Snapshot>("get_snapshot");
  }
  async sendPrompt(text: string) {
    await invoke("send_prompt", { text });
  }
  async cancelTurn() {
    await invoke("cancel_turn");
  }
  async answerQuestion(requestId: string, answers: Record<string, string[]>) {
    await invoke("answer_question", { requestId, answers });
  }
  async submitSecret(requestId: string, value: string | null) {
    await invoke("submit_secret", { requestId, value });
  }
  async webBounds(placementId: string, rect: WebRect | null) {
    await invoke("web_bounds", { placementId, rect });
  }
  async saveAudio(placementId: string, base64: string, mime: string, extra: { tool?: string; arg?: string; transcribe?: string; insert?: boolean; locale?: string } = {}) {
    // `action` carries the placement's audio.record action (tool/arg or §18 transcribe/insert); the kernel falls back to
    // the view's own action when it is absent.
    const action = { host: "audio.record", ...extra };
    await invoke("save_audio", { placementId, data: base64, mime, action });
  }
  async selectHarness(id: string) {
    await invoke("select_harness", { id });
  }
  async harnessLogin(id: string, method?: string) {
    await invoke("harness_login", { id, method: method ?? null });
  }
  async harnessLoginSubmit(id: string, kind: "code" | "apikey", value: string, provider?: string) {
    await invoke("harness_login_submit", { id, kind, value, provider: provider ?? null });
  }
  async respondPermission(requestId: string, optionId: string | null) {
    await invoke("respond_permission", { requestId, optionId });
  }
  uiAction(placementId: string, action: Action, input?: Record<string, unknown>) {
    return invoke<ActionResult>("ui_action", { placementId, action, input: input ?? {} });
  }
  iframeCall(module: string, tool: string, args: Record<string, unknown>) {
    return invoke<CallToolResult>("iframe_call", { module, tool, args });
  }
  async reportUiError(placementId: string, message: string) {
    await invoke("report_ui_error", { placementId, message });
  }
  onSnapshot = makeListener<Snapshot>("oa://snapshot");
  onChat = makeListener<ChatEvent>("oa://chat");
  onComposer = makeListener<ComposerEvent>("oa://composer");
  onNotify = makeListener<NotifyEvent>("oa://notify");
  channelSubscribe(placementId: string, topic: string) {
    return invoke<{ sub: number; topic: string; replay: ChannelMessage[] }>("channel_subscribe", { placementId, topic });
  }
  async channelUnsubscribe(sub: number) {
    await invoke("channel_unsubscribe", { sub });
  }
  channelPublish(placementId: string, message: { topic: string; data?: unknown; b64?: string }) {
    return invoke<unknown>("channel_publish", { placementId, message });
  }
  onChannel = makeListener<ChannelMessage>("oa://channel");
}

const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// The browser mock is only needed outside Tauri: load it lazily so it stays out of the main chunk.
export const kernel: Kernel = inTauri ? new TauriKernel() : new (await import("./mock")).MockKernel();
