import { useEffect, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import { Loader2 } from "lucide-react";
import { Icon } from "./Icon";
import { Anchor, ErrorBox, usePlacement } from "./PlacementView";
import { Inline } from "./Markdown";
import { useShell } from "./store";
import { useRecState } from "./audio";
import { resolveHostProps, streamTopics } from "./hostData";
import { useStreams } from "./channels";
import { SpecDropzone } from "./Dropzone";

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown, d = ""): string => (typeof v === "string" ? v : v == null ? d : String(v));
const num = (v: unknown): number | undefined => (typeof v === "number" && Number.isFinite(v) ? v : undefined);
const sp = (n: number | undefined): string | undefined => (n === undefined ? undefined : `calc(${n}px * var(--oa-space, 1))`);
const MAX_NODE_DEPTH = 40;

const ALIGN: Record<string, string> = { start: "flex-start", center: "center", end: "flex-end", stretch: "stretch" };
const JUSTIFY: Record<string, string> = { start: "flex-start", center: "center", end: "flex-end", between: "space-between" };

function useAction() {
  const { runAction } = useShell();
  const { placementId } = usePlacement();
  const [busy, setBusy] = useState(false);
  const run = async (action: unknown, input?: Obj): Promise<boolean> => {
    if (busy) return false;
    setBusy(true);
    try {
      return await runAction(placementId, action, input);
    } finally {
      setBusy(false);
    }
  };
  return { run, busy };
}

/** audio.record controls show a recording state while this placement is recording (with elapsed seconds). */
function useRecording(action: unknown): { on: boolean; secs: number } {
  const { placementId } = usePlacement();
  const rec = useRecState();
  const isRec = isObj(action) && action.host === "audio.record";
  const on = isRec && rec.recording === placementId;
  const [secs, setSecs] = useState(0);
  useEffect(() => {
    if (!on) return setSecs(0);
    const tick = () => setSecs(Math.floor((Date.now() - rec.startedAt) / 1000));
    tick();
    const t = setInterval(tick, 1000);
    return () => clearInterval(t);
  }, [on, rec.startedAt]);
  return { on, secs };
}

function Spinner() {
  return <Loader2 size={14} className="oa-spin" aria-hidden="true" />;
}

export function SpecRenderer({ tree }: { tree: unknown }) {
  return <NodeView node={tree} depth={0} />;
}

function Children({ nodes, depth }: { nodes: unknown; depth: number }): ReactNode {
  if (!Array.isArray(nodes)) return null;
  return nodes.map((c, i) => <NodeView key={i} node={c} depth={depth + 1} />);
}

function BadNode({ message }: { message: string }) {
  const { placementId } = usePlacement();
  return <ErrorBox placementId={placementId} report>{message}</ErrorBox>;
}

function NodeView({ node: raw, depth }: { node: unknown; depth: number }): ReactNode {
  const { snapshot } = useShell();
  const { placementId } = usePlacement();
  // §21 G1: `$stream` props subscribe on mount, unsubscribe on unmount
  const streams = useStreams(placementId, isObj(raw) ? streamTopics(raw) : []);
  if (depth > MAX_NODE_DEPTH) return <BadNode message="Spec tree too deep" />;
  if (!isObj(raw)) return <BadNode message={`Invalid spec node: ${JSON.stringify(raw)?.slice(0, 80) ?? String(raw)}`} />;
  const { node, errors } = resolveHostProps(raw, snapshot, streams.values);
  errors.push(...streams.errors);
  if (errors.length) return <BadNode message={errors.join("; ")} />;
  const type = str(node.type);
  switch (type) {
    case "stack": {
      const dir = node.direction === "row" ? "row" : "column";
      const style: CSSProperties = {
        flexDirection: dir,
        gap: sp(num(node.gap) ?? 8),
        padding: sp(num(node.padding)),
        alignItems: ALIGN[str(node.align)],
        justifyContent: JUSTIFY[str(node.justify)],
      };
      return <div className="oa-stack" style={style}><Children nodes={node.children} depth={depth} /></div>;
    }
    case "text": {
      const variant = str(node.variant, "body");
      const value = str(node.value);
      return variant === "mono" ? (
        <div className="oa-text oa-text-mono">{value}</div>
      ) : (
        <div className={`oa-text oa-text-${variant}`}><Inline text={value} /></div>
      );
    }
    case "button": return <SpecButton node={node} />;
    case "icon-button": return <SpecIconButton node={node} />;
    case "input": return <SpecInput node={node} />;
    case "toggle": return <SpecToggle node={node} />;
    case "list": return <SpecList node={node} />;
    case "card":
      return (
        <section className="oa-card">
          {node.title ? <h3 className="oa-card-title">{str(node.title)}</h3> : null}
          <div className="oa-card-body"><Children nodes={node.children} depth={depth} /></div>
        </section>
      );
    case "badge": {
      const tone = ["neutral", "accent", "success", "warning", "danger"].includes(str(node.tone)) ? str(node.tone) : "neutral";
      return <span className={`oa-badge oa-badge-${tone}`}>{str(node.label)}</span>;
    }
    case "progress": {
      const v = Math.min(1, Math.max(0, num(node.value) ?? 0));
      return (
        <div className="oa-progress" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(v * 100)}>
          <div className="oa-progress-bar" style={{ width: `${v * 100}%` }} />
        </div>
      );
    }
    case "image": return <SpecImage node={node} />;
    case "divider": return <hr className="oa-divider" />;
    case "slot": {
      const id = str(node.id);
      return id ? <Anchor name={`slot:${id}`} className="oa-slot" /> : <BadNode message="slot: missing id" />;
    }
    case "dropzone":
      return <SpecDropzone node={node}>{Array.isArray(node.children) && node.children.length ? <Children nodes={node.children} depth={depth} /> : undefined}</SpecDropzone>;
    default:
      return <BadNode message={`Unknown spec node type "${type || "(none)"}"`} />;
  }
}

function SpecButton({ node }: { node: Obj }) {
  const { run, busy } = useAction();
  const variant = ["primary", "secondary", "ghost", "danger"].includes(str(node.variant)) ? str(node.variant) : "secondary";
  const { on, secs } = useRecording(node.action);
  return (
    <button type="button" className={`oa-btn oa-btn-${variant}${on ? " is-recording" : ""}`} disabled={busy} aria-busy={busy} title={on ? `Recording ${secs}s — click to stop, Esc to cancel` : undefined} onClick={() => run(node.action)}>
      {busy ? <Spinner /> : on ? <span className="oa-recdot" aria-hidden="true" /> : node.icon ? <Icon name={str(node.icon)} size={15} /> : null}
      <span>{str(node.label)}</span>
    </button>
  );
}

function SpecIconButton({ node }: { node: Obj }) {
  const { run, busy } = useAction();
  const { on, secs } = useRecording(node.action);
  const active = node.active === true || on;
  const label = str(node.label, str(node.icon));
  return (
    <button
      type="button"
      className={`oa-iconbtn${active ? " is-active" : ""}${on ? " is-recording" : ""}`}
      title={on ? `Recording ${secs}s — click to stop, Esc to cancel` : label}
      aria-label={on ? `${label} (recording, ${secs} seconds)` : label}
      aria-pressed={node.active === undefined && !on ? undefined : active}
      disabled={busy}
      aria-busy={busy}
      onClick={() => run(node.action)}
    >
      {busy ? <Spinner /> : <Icon name={str(node.icon)} size={17} />}
      {on ? <span className="oa-recdot oa-recdot-corner" aria-hidden="true" /> : null}
    </button>
  );
}

function SpecInput({ node }: { node: Obj }) {
  const { run, busy } = useAction();
  const initial = str(node.value);
  const [value, setValue] = useState(initial);
  useEffect(() => setValue(initial), [initial]);
  const submit = async () => {
    if (!node.submit) return;
    if (await run(node.submit, { value })) setValue("");
  };
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter" && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void submit();
    }
  };
  return (
    <div className="oa-input-wrap">
      <input
        className="oa-input"
        name={str(node.name)}
        placeholder={str(node.placeholder)}
        aria-label={str(node.placeholder) || str(node.name) || "Input"}
        value={value}
        disabled={busy}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={onKey}
      />
      {busy ? <Spinner /> : null}
    </div>
  );
}

function SpecToggle({ node }: { node: Obj }) {
  const { run, busy } = useAction();
  const prop = node.value === true;
  const [on, setOn] = useState(prop);
  useEffect(() => setOn(prop), [prop]);
  return (
    <label className="oa-toggle">
      <button
        type="button"
        role="switch"
        aria-checked={on}
        aria-label={str(node.label)}
        className={`oa-switch${on ? " is-on" : ""}`}
        disabled={busy}
        onClick={async () => {
          const next = !on;
          setOn(next);
          const ok = await run(node.action, { value: next });
          if (!ok) setOn(!next); // revert; on success the view (prop) is the source of truth
        }}
      >
        <span className="oa-switch-knob" />
      </button>
      <span className="oa-toggle-label">{str(node.label)}</span>
      {busy ? <Spinner /> : null}
    </label>
  );
}

function SpecList({ node }: { node: Obj }) {
  const items = Array.isArray(node.items) ? node.items.filter(isObj) : [];
  if (!items.length) return <div className="oa-text oa-text-muted">{str(node.empty, "Nothing here yet.")}</div>;
  return (
    <ul className="oa-list">
      {items.map((it, i) => (
        <ListItem key={str(it.id, String(i))} item={it} />
      ))}
    </ul>
  );
}

function ListItem({ item }: { item: Obj }) {
  const { run, busy } = useAction();
  const body = (
    <>
      {item.icon ? <span className="oa-list-icon"><Icon name={str(item.icon)} size={16} /></span> : null}
      <span className="oa-list-text">
        <span className="oa-list-title">{str(item.title)}</span>
        {item.subtitle ? <span className="oa-list-sub">{str(item.subtitle)}</span> : null}
      </span>
      {busy ? <Spinner /> : null}
    </>
  );
  return (
    <li>
      {item.action ? (
        <button type="button" className={`oa-list-row is-action${item.active === true ? " is-active" : ""}`} aria-current={item.active === true ? "true" : undefined} disabled={busy} onClick={() => run(item.action)}>{body}</button>
      ) : (
        <div className={`oa-list-row${item.active === true ? " is-active" : ""}`} aria-current={item.active === true ? "true" : undefined}>{body}</div>
      )}
    </li>
  );
}

function SpecImage({ node }: { node: Obj }) {
  const src = str(node.src);
  const ok = /^(https?:\/\/|oamod:\/\/)/i.test(src);
  if (!ok) return <BadNode message={`image: unsupported src "${src.slice(0, 60)}"`} />;
  const h = num(node.height);
  return <img className="oa-image" src={src} alt={str(node.alt)} style={h ? { height: h } : undefined} />;
}
