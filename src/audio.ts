// §15.1 host microphone: getUserMedia + MediaRecorder in the main webview; bytes go to the kernel via save_audio.
import { useSyncExternalStore } from "react";
import { kernel } from "./kernel";

export interface RecState {
  /** placement currently recording */
  recording: string | null;
  startedAt: number;
}

let state: RecState = { recording: null, startedAt: 0 };
const subs = new Set<() => void>();
const set = (s: RecState) => {
  state = s;
  subs.forEach((f) => f());
};

export const recState = (): RecState => state;

export function useRecState(): RecState {
  return useSyncExternalStore(
    (f) => (subs.add(f), () => void subs.delete(f)),
    () => state,
  );
}

let rec: MediaRecorder | null = null;
let stream: MediaStream | null = null;
let chunks: Blob[] = [];
let cancelled = false;
let stopped: Promise<{ blob: Blob; mime: string } | null> | null = null;

function describeError(err: unknown): string {
  const name = (err as { name?: string })?.name;
  if (name === "NotAllowedError" || name === "SecurityError") return "Microphone access was denied. Allow it for OpenApp in System Settings > Privacy > Microphone.";
  if (name === "NotFoundError" || name === "OverconstrainedError") return "No microphone was found.";
  if (name === "NotReadableError") return "The microphone is busy or unavailable.";
  return `Could not record: ${(err as Error)?.message ?? String(err)}`;
}

function release() {
  stream?.getTracks().forEach((t) => t.stop());
  stream = null;
  rec = null;
  chunks = [];
  window.removeEventListener("keydown", onKey, true);
  set({ recording: null, startedAt: 0 });
}

function onKey(e: KeyboardEvent) {
  if (e.key === "Escape" && rec) {
    e.preventDefault();
    cancelRecording();
  }
}

export function cancelRecording() {
  if (!rec) return;
  cancelled = true;
  if (rec.state !== "inactive") rec.stop();
  release();
}

function toBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const fr = new FileReader();
    fr.onload = () => resolve(String(fr.result).split(",", 2)[1] ?? "");
    fr.onerror = () => reject(fr.error ?? new Error("read failed"));
    fr.readAsDataURL(blob);
  });
}

export interface RecordAction {
  tool?: string;
  arg?: string;
  /** §18: "apple" = the kernel transcribes on the Mac (SpeechAnalyzer) */
  transcribe?: string;
  /** §18: with transcribe, insert the transcript into the composer */
  insert?: boolean;
  locale?: string;
}

/** §18: AAC/m4a first (AVFoundation reads it directly), webm/opus only as a fallback. */
export function pickMime(): string | undefined {
  for (const m of ["audio/mp4;codecs=mp4a.40.2", "audio/mp4", "audio/aac", "audio/webm;codecs=opus", "audio/webm"]) {
    if (MediaRecorder.isTypeSupported(m)) return m;
  }
  return undefined;
}

/**
 * First call starts recording for `placementId`; second call stops, uploads and resolves after the kernel finished.
 * Throws a user-readable Error on failure. Only one recording at a time.
 */
export async function toggleRecording(placementId: string, action: RecordAction): Promise<boolean> {
  if (state.recording && state.recording !== placementId) throw new Error("Another recording is already in progress.");

  if (state.recording === placementId && rec) {
    // stop + upload
    const r = rec;
    const mime = r.mimeType || "audio/mp4";
    cancelled = false;
    stopped = new Promise((resolve) => {
      r.onstop = () => resolve(cancelled ? null : { blob: new Blob(chunks, { type: mime }), mime });
    });
    if (r.state !== "inactive") r.stop();
    const result = await stopped;
    release();
    if (!result || result.blob.size === 0) return true;
    const b64 = await toBase64(result.blob);
    await kernel.saveAudio(placementId, b64, result.mime, action);
    return true;
  }

  if (!navigator.mediaDevices?.getUserMedia || typeof MediaRecorder === "undefined") {
    throw new Error("Microphone recording is not available in this window.");
  }
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (err) {
    throw new Error(describeError(err));
  }
  try {
    const pref = pickMime();
    rec = pref ? new MediaRecorder(stream, { mimeType: pref }) : new MediaRecorder(stream);
    chunks = [];
    cancelled = false;
    rec.ondataavailable = (e) => {
      if (e.data.size) chunks.push(e.data);
    };
    rec.start(250);
  } catch (err) {
    release();
    throw new Error(describeError(err));
  }
  window.addEventListener("keydown", onKey, true);
  set({ recording: placementId, startedAt: Date.now() });
  return true;
}
