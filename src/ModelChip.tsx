// §23 model chip: the discovered model of the next turn, next to the mode chip. The popover lists what the
// signed-in account offers (never a hardcoded list), the effort steps of the chosen model, and the slots.
import { useEffect, useMemo, useRef, useState } from "react";
import { Check, ChevronDown, Cpu, RefreshCw, Zap } from "lucide-react";
import type { ModelInfo, Mode } from "./contract";
import { useShell } from "./store";

/** `xhigh` → "Extra high" (the harness's own effort names otherwise, capitalised). */
export function effortLabel(e: string): string {
  const v = e.toLowerCase();
  if (v === "xhigh") return "Extra high";
  if (v === "none" || v === "off") return "Off";
  return v.charAt(0).toUpperCase() + v.slice(1);
}

export function ModelChip({ mode }: { mode: Mode }) {
  const { snapshot, runAction } = useShell();
  const { agent } = snapshot;
  const prefs = snapshot.modelPrefs ?? null;
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const catalog = useMemo(
    () => (snapshot.models ?? []).filter((m) => m.harness === agent.harness && !m.hidden),
    [snapshot.models, agent.harness],
  );
  const current = catalog.find((m) => m.id === agent.model) ?? null;

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (root.current && !root.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  if (!agent.harness || agent.status === "unselected") return null;

  const act = async (a: Record<string, unknown>) => {
    setBusy(true);
    try {
      await runAction("chat", a);
    } finally {
      setBusy(false);
    }
  };
  const pickModel = (m: ModelInfo) => void act({ host: "model.set", model: m.id, slot: "current" });
  const pickEffort = (e: string) => void act({ host: "model.set", effort: e, slot: "current" });

  const label = agent.modelName ? `${agent.modelName}${agent.effort ? ` · ${effortLabel(agent.effort)}` : ""}` : "Default model";
  const strongName = prefs?.strong ? catalog.find((m) => m.id === prefs.strong!.model)?.displayName ?? prefs.strong.model : null;
  const buildName = prefs?.build ? catalog.find((m) => m.id === prefs.build!.model)?.displayName ?? prefs.build.model : null;
  const isStrong = agent.tier === "strong";

  return (
    <div className="oa-model" ref={root}>
      <button
        type="button"
        className={`oa-mode oa-model-chip${isStrong ? " is-strong" : ""}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        title={isStrong && prefs?.escalated ? `Strong model for this task: ${prefs.escalated}` : "Model and effort for this mode"}
      >
        {isStrong ? <Zap size={12} aria-hidden="true" /> : <Cpu size={12} aria-hidden="true" />}
        <span className="oa-model-label">{label}</span>
        <ChevronDown size={12} aria-hidden="true" />
      </button>
      {open ? (
        <div className="oa-model-pop" role="dialog" aria-label="Model">
          <div className="oa-model-head">
            <span>{mode === "build" ? "Build mode model" : "Chat mode model"}</span>
            <button type="button" className="oa-icon-btn" title="Discover the models again" aria-label="Refresh models" disabled={busy} onClick={() => void act({ host: "models.refresh" })}>
              <RefreshCw size={13} aria-hidden="true" className={busy ? "oa-spin" : undefined} />
            </button>
          </div>
          {catalog.length === 0 ? (
            <p className="oa-model-empty">{prefs?.error ? `No models discovered (${prefs.error}).` : "No models discovered yet: the provider uses its own default."}</p>
          ) : (
            <ul className="oa-model-list" role="listbox" aria-label="Models">
              {catalog.map((m) => {
                const on = m.id === agent.model;
                return (
                  <li key={m.id}>
                    <button type="button" role="option" aria-selected={on} className={`oa-model-item${on ? " is-on" : ""}`} disabled={busy} onClick={() => pickModel(m)}>
                      <span className="oa-model-name">
                        {m.displayName}
                        {prefs?.strong?.model === m.id ? <span className="oa-model-tag">strong</span> : null}
                        {m.isDefault ? <span className="oa-model-tag is-muted">default</span> : null}
                      </span>
                      {m.description ? <span className="oa-model-desc">{m.description}</span> : null}
                      {on ? <Check size={14} className="oa-model-check" aria-hidden="true" /> : null}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
          {current && current.efforts.length > 1 ? (
            <div className="oa-model-effort" role="radiogroup" aria-label="Effort">
              {current.efforts.map((e) => (
                <button key={e} type="button" role="radio" aria-checked={agent.effort === e} className={`oa-seg${agent.effort === e ? " is-on" : ""}`} disabled={busy} onClick={() => pickEffort(e)}>
                  {effortLabel(e)}
                </button>
              ))}
            </div>
          ) : null}
          {current ? (
            <div className="oa-model-actions">
              <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => void act({ host: "model.set", model: current.id, effort: agent.effort ?? undefined, slot: "build" })}>
                Set as build default
              </button>
              <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => void act({ host: "model.set", model: current.id, effort: agent.effort ?? undefined, slot: "strong" })}>
                Set as strong
              </button>
            </div>
          ) : null}
          <div className="oa-model-foot">
            <span>Build: {buildName ?? "same as chat"} · Strong: {strongName ?? "not set"}</span>
            <label className="oa-model-policy">
              <input type="checkbox" checked={prefs?.policy !== "auto"} disabled={busy} onChange={(e) => void act({ host: "model.policy", policy: e.target.checked ? "ask" : "auto" })} />
              Ask before switching to strong
            </label>
          </div>
        </div>
      ) : null}
    </div>
  );
}
