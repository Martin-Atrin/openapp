import { Fragment, memo, useEffect, useRef, useState, type ReactNode } from "react";
import { AlertCircle, Check, ChevronRight, Hammer, Loader2, ShieldCheck, Undo2, X } from "lucide-react";
import type { ChatItem } from "./contract";
import { kernel } from "./kernel";
import { Markdown } from "./Markdown";
import { InlineList, useCloseInline, useInline } from "./Presentations";
import { QuestionCard } from "./Question";
import { SecretCard, SystemLink } from "./Secret";
import { useShell } from "./store";

/** Render items as today: tool items grouped into chip rows, everything else as its own row. */
function renderItems(chat: ChatItem[]): ReactNode[] {
  const rows: ReactNode[] = [];
  for (let i = 0; i < chat.length; i++) {
    const it = chat[i]!;
    if (it.kind === "tool") {
      const group: ChatItem[] = [it];
      while (chat[i + 1]?.kind === "tool") group.push(chat[++i]!);
      rows.push(<ToolGroup key={group[0]!.id} items={group} />);
    } else {
      rows.push(<ChatRow key={it.id} item={it} />);
    }
  }
  return rows;
}

type Seg = { type: "chat"; items: ChatItem[] } | { type: "build"; key: string; turnId?: string; title: string; items: ChatItem[] };

/**
 * §19.2: one build card per build-mode turn. A turn starts at a build-mode user item (or an item with mode "build"
 * and a turnId) and lasts while items share its turnId; without a turnId it lasts until the next user item.
 */
function segment(chat: ChatItem[]): Seg[] {
  const segs: Seg[] = [];
  let lastUser = "";
  for (let i = 0; i < chat.length; i++) {
    const it = chat[i]!;
    if (it.kind === "user") lastUser = it.text ?? "";
    const startsBuild = it.mode === "build" && (it.kind === "user" || !!it.turnId);
    if (!startsBuild) {
      const last = segs[segs.length - 1];
      if (last?.type === "chat") last.items.push(it);
      else segs.push({ type: "chat", items: [it] });
      continue;
    }
    const turnId = it.turnId;
    const items: ChatItem[] = [it];
    while (i + 1 < chat.length) {
      const n = chat[i + 1]!;
      if (n.kind === "user") break;
      if (turnId && n.turnId && n.turnId !== turnId) break;
      if (turnId && !n.turnId && n.mode && n.mode !== "build") break;
      items.push(n);
      i++;
    }
    segs.push({ type: "build", key: it.id, turnId, title: lastUser || "Build", items });
  }
  return segs;
}

export function Transcript({ chat }: { chat: ChatItem[] }) {
  const { snapshot } = useShell();
  const inline = useInline();
  const close = useCloseInline();
  const segs = segment(chat);
  const working = snapshot.agent.status === "working";
  const ids = new Set(chat.map((c) => c.id));
  const at = (id: string) => inline.filter((p) => p.after === id);
  const tail = inline.filter((p) => !p.after || !ids.has(p.after));

  return (
    <div className="oa-transcript" role="log" aria-live="polite" aria-label="Conversation">
      {segs.map((sg, i) => {
        if (sg.type === "build") {
          const here = sg.items.flatMap((it) => at(it.id));
          return (
            <Fragment key={sg.key}>
              <BuildCard seg={sg} live={working && i === segs.length - 1} />
              {here.length ? <InlineList list={here} onClose={close} /> : null}
            </Fragment>
          );
        }
        // chat items: flush the run before each insertion point
        const out: ReactNode[] = [];
        let run: ChatItem[] = [];
        for (const it of sg.items) {
          run.push(it);
          const ins = at(it.id);
          if (ins.length) {
            out.push(<Fragment key={`r${run[0]!.id}`}>{renderItems(run)}</Fragment>);
            out.push(<InlineList key={`i${it.id}`} list={ins} onClose={close} />);
            run = [];
          }
        }
        if (run.length) out.push(<Fragment key={`r${run[0]!.id}`}>{renderItems(run)}</Fragment>);
        return <Fragment key={sg.items[0]!.id}>{out}</Fragment>;
      })}
      {tail.length ? <InlineList list={tail} onClose={close} /> : null}
    </div>
  );
}

/** Plain-words status for a tool item (small mapping; fallback "Working…"). */
export function describeStep(t: ChatItem): string {
  const raw = (t.command ?? t.title ?? "").trim();
  const s = raw.toLowerCase();
  if (/module_load|module load/.test(s)) return "Loading module…";
  if (/openapp_layout/.test(s)) return "Arranging the layout…";
  if (/openapp_theme/.test(s)) return "Updating the theme…";
  if (/openapp_describe|openapp_snapshot/.test(s)) return "Checking the app…";
  if (/openapp_call/.test(s)) return "Testing the module…";
  if (/openapp_undo/.test(s)) return "Reverting changes…";
  if (/\b(npm|pnpm|yarn|bun)\s+(i|install|add)\b|pip3?\s+install|cargo\s+add/.test(s)) return "Installing packages…";
  if (/\b(ffmpeg|whisper)/.test(s)) return "Processing audio…";
  if (/\b(test|vitest|jest|tsc|build|lint)\b/.test(s)) return "Running checks…";
  const m = /^(edit|update|write|create|add|modify)\b\s+(.*)$/.exec(s);
  if (m) {
    const path = (m[2] ?? "").split(/\s+/)[0] ?? "";
    const base = path.split("/").pop() ?? path;
    const view = /views\/([^/.]+)\.(json|html)$/.exec(path);
    if (view) return `${m[1] === "create" || m[1] === "write" ? "Writing" : "Editing"} the ${view[1]} view…`;
    if (/^server\./.test(base)) return "Writing the server…";
    if (base === "openapp.json") return "Updating the module manifest…";
    return base ? `${m[1] === "create" || m[1] === "write" ? "Writing" : "Editing"} ${base}…` : "Editing files…";
  }
  if (/^(read|cat|ls|grep|find|search|glob|open)\b/.test(s)) return "Reading the project…";
  return "Working…";
}

function liveStatus(items: ChatItem[]): { text: string; wait: boolean } {
  if (items.some((i) => (i.kind === "question" || i.kind === "permission" || i.kind === "secret") && i.resolved === undefined)) {
    return { text: "Waiting for your answer…", wait: true };
  }
  for (let i = items.length - 1; i >= 0; i--) {
    const it = items[i]!;
    if (it.kind === "tool") return { text: describeStep(it), wait: false };
    if (it.kind === "thought") return { text: "Thinking…", wait: false };
    if (it.kind === "agent" && it.text) return { text: "Working…", wait: false };
  }
  return { text: "Working…", wait: false };
}

/** §19.2 build card: request, one live status line, inline questions/permissions, final summary, Undo, details. */
function BuildCard({ seg, live }: { seg: Extract<Seg, { type: "build" }>; live: boolean }) {
  const { runAction } = useShell();
  const [details, setDetails] = useState(false);
  const [undo, setUndo] = useState<"idle" | "busy" | "done">("idle");
  const body = seg.items.filter((i) => i.kind !== "user");
  const inline = body.filter((i) => i.kind === "question" || i.kind === "permission" || i.kind === "secret" || i.kind === "error" || (i.kind === "system" && (!!i.url || !!i.code || !!i.modelSwitch)));
  const agents = body.filter((i) => i.kind === "agent" && i.text);
  const summary = live ? undefined : agents[agents.length - 1];
  const status = liveStatus(body);
  const canUndo = !live && !!seg.turnId;
  const log = body.filter((i) => !inline.includes(i) || i.kind === "error");

  return (
    <section className="oa-build" aria-label={`Build: ${seg.title}`}>
      <div className="oa-build-head"><Hammer size={15} aria-hidden="true" /><div className="oa-build-title">{seg.title}</div></div>
      {live ? (
        <div className={`oa-build-status${status.wait ? " is-wait" : ""}`} role="status">
          {status.wait ? <ShieldCheck size={14} aria-hidden="true" /> : <Loader2 size={14} className="oa-spin" aria-hidden="true" />}
          <span>{status.text}</span>
        </div>
      ) : null}
      {inline.map((i) => <ChatRow key={i.id} item={i} />)}
      {summary ? <div className="oa-msg-agent"><Markdown text={summary.text ?? ""} /></div> : null}
      {!live && !summary && !inline.length ? <div className="oa-build-done">Finished.</div> : null}
      {undo === "done" ? <div className="oa-build-done">Undone.</div> : null}
      {!live || details ? (
        <div className="oa-build-actions">
          {canUndo && undo !== "done" ? (
            <button
              type="button"
              className="oa-btn oa-btn-secondary"
              disabled={undo === "busy"}
              onClick={async () => {
                setUndo("busy");
                const ok = await runAction("chat", { host: "turn.undo", turnId: seg.turnId });
                setUndo(ok ? "done" : "idle");
              }}
            >
              {undo === "busy" ? <Loader2 size={14} className="oa-spin" aria-hidden="true" /> : <Undo2 size={14} aria-hidden="true" />}
              Undo
            </button>
          ) : null}
          <button type="button" className="oa-btn oa-btn-ghost" aria-expanded={details} onClick={() => setDetails((d) => !d)}>
            <ChevronRight size={14} className={details ? "oa-rot" : undefined} aria-hidden="true" />
            {details ? "Hide details" : "Show details"}
          </button>
        </div>
      ) : (
        <div className="oa-build-actions">
          <button type="button" className="oa-btn oa-btn-ghost" aria-expanded={details} onClick={() => setDetails((d) => !d)}>
            <ChevronRight size={14} aria-hidden="true" /> Show details
          </button>
        </div>
      )}
      {details ? <div className="oa-build-details">{renderItems(log)}</div> : null}
    </section>
  );
}

const ChatRow = memo(function ChatRow({ item }: { item: ChatItem }) {
  switch (item.kind) {
    case "user":
      return <div className="oa-msg oa-msg-user"><div className="oa-bubble">{item.text}</div></div>;
    case "agent":
      return item.text ? <div className="oa-msg oa-msg-agent"><Markdown text={item.text} /></div> : null;
    case "thought":
      return <Thought item={item} />;
    case "permission":
      return <PermissionCard item={item} />;
    case "question":
      return <QuestionCard item={item} />;
    case "secret":
      return <SecretCard item={item} />;
    case "system":
      return item.url || item.code ? <SystemLink item={item} /> : <div className="oa-line">{item.text}</div>;
    case "error":
      return <div className="oa-line oa-line-error" role="alert"><AlertCircle size={14} aria-hidden="true" />{item.text}</div>;
    default:
      return null;
  }
});

function Thought({ item }: { item: ChatItem }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="oa-thought">
      <button type="button" className="oa-thought-toggle" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        <ChevronRight size={13} className={open ? "oa-rot" : undefined} aria-hidden="true" />
        Thought
      </button>
      {open ? <div className="oa-thought-body">{item.text}</div> : null}
    </div>
  );
}

function ToolGroup({ items }: { items: ChatItem[] }) {
  return (
    <div className="oa-tools">
      {items.map((t) => (
        <ToolChip key={t.id} item={t} />
      ))}
    </div>
  );
}

function lastLine(out?: string): string {
  const lines = (out ?? "").trimEnd().split("\n");
  return lines[lines.length - 1] ?? "";
}

/** A tool chip. Command items (§15.3) expand to cwd, full command, output and exit code. */
function ToolChip({ item: t }: { item: ChatItem }) {
  const [open, setOpen] = useState(false);
  const isCmd = t.command !== undefined || t.output !== undefined || t.exitCode !== undefined;
  const failed = t.status === "failed" || (typeof t.exitCode === "number" && t.exitCode !== 0);
  const label = t.command ?? t.title ?? "tool";
  const outRef = useRef<HTMLPreElement>(null);
  useEffect(() => {
    // Live output: keep the tail in view while running.
    const el = outRef.current;
    if (open && el && t.status === "in_progress") el.scrollTop = el.scrollHeight;
  }, [open, t.output, t.status]);

  const status = failed && t.status !== "failed" ? "failed" : t.status ?? "pending";
  const chip = (
    <>
      <StatusIcon status={status} />
      <span className="oa-chip-text">{label}</span>
    </>
  );
  return (
    <>
      {isCmd ? (
        <button type="button" className={`oa-chip oa-chip-cmd oa-chip-${status}`} aria-expanded={open} title={`${label} — ${status}`} onClick={() => setOpen((o) => !o)}>
          {chip}
          <ChevronRight size={11} className={open ? "oa-rot" : undefined} aria-hidden="true" />
        </button>
      ) : (
        <span className={`oa-chip oa-chip-${status}`} title={`${label} — ${status}`}>{chip}</span>
      )}
      {isCmd && failed && !open ? (
        <div className="oa-cmd-fail">{t.command === undefined && t.exitCode == null ? "failed" : `exit ${t.exitCode ?? "?"}`}{lastLine(t.output) ? ` · ${lastLine(t.output)}` : ""}</div>
      ) : null}
      {isCmd && open ? (
        <div className="oa-cmd-detail">
          {t.cwd ? <div className="oa-cmd-meta">cwd: {t.cwd}</div> : null}
          {t.command ? <div className="oa-cmd-cmd">$ {t.command}</div> : null}
          <pre className="oa-cmd-out" ref={outRef} tabIndex={0} aria-label="Command output">{t.output || (t.status === "in_progress" ? "(running…)" : "(no output)")}</pre>
          {t.exitCode !== undefined && t.exitCode !== null ? <div className={`oa-cmd-exit${t.exitCode !== 0 ? " is-fail" : ""}`}>exit code {t.exitCode}</div> : null}
        </div>
      ) : null}
    </>
  );
}

function StatusIcon({ status }: { status?: string }) {
  if (status === "completed") return <Check size={12} aria-label="completed" />;
  if (status === "failed") return <X size={12} aria-label="failed" />;
  if (status === "in_progress") return <Loader2 size={12} className="oa-spin" aria-label="running" />;
  return <span className="oa-dot-pending" aria-label="pending" />;
}

/** Kernel chrome: looks different from module UI on purpose (shield + "OpenApp"). */
function PermissionCard({ item }: { item: ChatItem }) {
  const { toast } = useShell();
  const [busy, setBusy] = useState(false);
  const resolved = item.resolved !== undefined; // null = cancelled, string = chosen option
  const chosen = typeof item.resolved === "string" ? item.options?.find((o) => o.optionId === item.resolved) : undefined;

  const respond = async (optionId: string | null) => {
    setBusy(true);
    try {
      await kernel.respondPermission(item.id, optionId);
    } catch (err) {
      toast(String(err), "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className={`oa-perm${resolved ? " is-resolved" : ""}`} role="group" aria-label="Permission request">
      <div className="oa-perm-head"><ShieldCheck size={15} aria-hidden="true" /><span>OpenApp</span><em>asks permission</em></div>
      <div className="oa-perm-title">{item.title ?? item.text ?? "The agent requests permission."}</div>
      {item.title && item.text ? <div className="oa-perm-text">{item.text}</div> : null}
      {item.details ? <ConsentDetails details={item.details} /> : null}
      {item.diff ? <DiffSection diff={item.diff} /> : null}
      {resolved ? (
        <div className="oa-perm-result">
          {chosen ? <><Check size={13} aria-hidden="true" /> {chosen.name}</> : <><X size={13} aria-hidden="true" /> Cancelled</>}
        </div>
      ) : (
        <div className="oa-perm-actions">
          {(item.options ?? []).map((o) => (
            <button
              key={o.optionId}
              type="button"
              disabled={busy}
              className={`oa-btn ${o.kind === "delete" ? "oa-btn-danger" : o.kind.startsWith("allow") ? "oa-btn-primary" : "oa-btn-secondary"}`}
              onClick={() => respond(o.optionId)}
            >
              {o.name}
            </button>
          ))}
          <button type="button" disabled={busy} className="oa-btn oa-btn-ghost" onClick={() => respond(null)}>Dismiss</button>
        </div>
      )}
    </div>
  );
}

/** Host-module consent analysis: summary, frameworks and flagged calls as chips. */
function ConsentDetails({ details }: { details: NonNullable<ChatItem["details"]> }) {
  const frameworks = details.frameworks ?? [];
  const calls = details.calls ?? [];
  return (
    <div className="oa-consent">
      {details.summary ? <div className="oa-consent-summary">{details.summary}</div> : null}
      {frameworks.length ? (
        <div className="oa-consent-row" aria-label="Frameworks">
          {frameworks.map((f) => <span key={f} className="oa-tag">{f}</span>)}
        </div>
      ) : null}
      {calls.length ? (
        <div className="oa-consent-row" aria-label="Flagged calls">
          {calls.map((c) => <span key={c} className="oa-tag oa-tag-warn" title="Flagged call"><AlertCircle size={11} aria-hidden="true" />{c}</span>)}
        </div>
      ) : null}
    </div>
  );
}

function DiffSection({ diff }: { diff: string }) {
  const [open, setOpen] = useState(false);
  const lines = diff.replace(/\n$/, "").split("\n");
  return (
    <div className="oa-diff">
      <button type="button" className="oa-thought-toggle" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        <ChevronRight size={13} className={open ? "oa-rot" : undefined} aria-hidden="true" />
        {open ? "Hide source changes" : "Show source changes"}
      </button>
      {open ? (
        <pre className="oa-diff-body" tabIndex={0} aria-label="Source changes">
          {lines.map((l, i) => {
            const cls = l.startsWith("+++") || l.startsWith("---") || l.startsWith("diff ") || l.startsWith("index ") ? "meta"
              : l.startsWith("@@") ? "hunk" : l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : "ctx";
            return <span key={i} className={`oa-dl oa-dl-${cls}`}>{l || " "}{"\n"}</span>;
          })}
        </pre>
      ) : null}
    </div>
  );
}
