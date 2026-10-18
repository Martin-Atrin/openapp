// §21 G1 live channels in the shell: one kernel subscription per (placement, topic), shared by every local
// callback; a short local ring so late local subscribers get the recent messages too.
import { useEffect, useState } from "react";
import type { ChannelMessage } from "./contract";
import { kernel } from "./kernel";

type Cb = (m: ChannelMessage) => void;
interface Entry {
  key: string;
  sub: number | null;
  cbs: Set<Cb>;
  ring: ChannelMessage[];
  error?: string;
  errCbs: Set<(e: string) => void>;
  readyCbs: Set<() => void>;
}

const RING = 64;
const byKey = new Map<string, Entry>();
const bySub = new Map<number, Entry>();
let listening: Promise<unknown> | null = null;

function deliver(e: Entry, m: ChannelMessage) {
  e.ring.push(m);
  if (e.ring.length > RING) e.ring.shift();
  e.cbs.forEach((cb) => {
    try {
      cb(m);
    } catch (err) {
      console.error("channel callback failed", err);
    }
  });
}

function ensureListener() {
  listening ??= kernel.onChannel((m) => {
    const e = typeof m.sub === "number" ? bySub.get(m.sub) : undefined;
    if (e) deliver(e, m);
  });
}

/** Subscribe `cb` to `topic` as the placement's module. Returns the unsubscribe function. */
export function subscribe(placementId: string, topic: string, cb: Cb, onError?: (e: string) => void, onReady?: () => void): () => void {
  ensureListener();
  const key = `${placementId}\n${topic}`;
  let e = byKey.get(key);
  if (!e) {
    const entry: Entry = { key, sub: null, cbs: new Set(), ring: [], errCbs: new Set(), readyCbs: new Set() };
    e = entry;
    byKey.set(key, entry);
    kernel
      .channelSubscribe(placementId, topic)
      .then((r) => {
        if (byKey.get(key) !== entry) {
          void kernel.channelUnsubscribe(r.sub);
          return;
        }
        entry.sub = r.sub;
        bySub.set(r.sub, entry);
        entry.readyCbs.forEach((f) => f());
        entry.readyCbs.clear();
        r.replay.forEach((m) => deliver(entry, m));
      })
      .catch((err) => {
        entry.error = String(err);
        entry.errCbs.forEach((f) => f(entry.error!));
        if (byKey.get(key) === entry) byKey.delete(key);
      });
  } else {
    if (e.sub !== null) onReady?.();
    e.ring.forEach((m) => cb(m));
    if (e.error && onError) onError(e.error);
  }
  if (onReady && e.sub === null) e.readyCbs.add(onReady);
  e.cbs.add(cb);
  if (onError) e.errCbs.add(onError);
  const entry = e;
  return () => {
    entry.cbs.delete(cb);
    if (onError) entry.errCbs.delete(onError);
    if (entry.cbs.size) return;
    if (byKey.get(key) === entry) byKey.delete(key);
    if (entry.sub !== null) {
      bySub.delete(entry.sub);
      void kernel.channelUnsubscribe(entry.sub);
    }
  };
}

export function publish(placementId: string, topic: string, data: unknown, b64?: string) {
  return kernel.channelPublish(placementId, b64 !== undefined ? { topic, b64 } : { topic, data });
}

/** Latest message per topic for the given topics (re-renders on every message); errors per topic. */
export function useStreams(placementId: string, topics: string[]): { values: Record<string, ChannelMessage | undefined>; errors: string[] } {
  const key = topics.join("\n");
  const [values, setValues] = useState<Record<string, ChannelMessage | undefined>>({});
  const [errors, setErrors] = useState<string[]>([]);
  useEffect(() => {
    if (!key) return;
    const list = key.split("\n");
    const uns = list.map((t) =>
      subscribe(
        placementId,
        t,
        (m) => setValues((v) => ({ ...v, [t]: m })),
        (err) => setErrors((x) => (x.includes(err) ? x : [...x, err])),
      ),
    );
    return () => {
      uns.forEach((u) => u());
      setErrors([]);
    };
  }, [placementId, key]);
  return { values, errors };
}
