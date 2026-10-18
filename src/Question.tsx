import { useId, useState } from "react";
import { Check, ShieldCheck, X } from "lucide-react";
import type { ChatItem, Question } from "./contract";
import { kernel } from "./kernel";
import { useShell } from "./store";

const OTHER = "__other__";

interface Draft {
  choice: string | null; // option label, OTHER, or null
  text: string; // Other / free text
}

function answerOf(q: Question, d: Draft | undefined): string[] {
  if (!d) return [];
  if (q.options?.length) {
    if (d.choice === OTHER) return d.text.trim() ? [d.text.trim()] : [];
    return d.choice ? [d.choice] : [];
  }
  return d.text.trim() ? [d.text.trim()] : [];
}

/** Kernel chrome (shield + "OpenApp"), like the permission card. */
export function QuestionCard({ item }: { item: ChatItem }) {
  const { toast } = useShell();
  const uid = useId();
  const qs = item.questions ?? [];
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [busy, setBusy] = useState(false);
  const [submitted, setSubmitted] = useState<Record<string, string[]> | null>(null);
  const resolved = item.resolved !== undefined;

  const set = (id: string, patch: Partial<Draft>) =>
    setDrafts((d) => ({ ...d, [id]: { ...(d[id] ?? { choice: null, text: "" }), ...patch } }));
  const complete = qs.every((q) => answerOf(q, drafts[q.id]).length > 0);

  const send = async (answers: Record<string, string[]>) => {
    setBusy(true);
    try {
      if (Object.keys(answers).length) setSubmitted(answers);
      await kernel.answerQuestion(item.id, answers);
    } catch (err) {
      setSubmitted(null);
      toast(String(err), "error");
    } finally {
      setBusy(false);
    }
  };
  const submit = () => {
    if (!complete || busy) return;
    const answers: Record<string, string[]> = {};
    for (const q of qs) answers[q.id] = answerOf(q, drafts[q.id]);
    void send(answers);
  };

  if (resolved) {
    const given = item.answers ?? (typeof item.resolved === "object" && item.resolved ? item.resolved : submitted);
    return (
      <div className="oa-perm is-resolved oa-ask" role="group" aria-label="Question (answered)">
        <div className="oa-perm-head"><ShieldCheck size={15} aria-hidden="true" /><span>OpenApp</span><em>asked</em></div>
        {given ? (
          <dl className="oa-ask-done">
            {qs.map((q) => (
              <div key={q.id}>
                <dt>{q.header || q.question}</dt>
                <dd>{q.isSecret ? "••••••" : (given[q.id] ?? []).join(", ") || "—"}</dd>
              </div>
            ))}
          </dl>
        ) : (
          <div className="oa-perm-result"><X size={13} aria-hidden="true" /> Cancelled</div>
        )}
        {given ? <div className="oa-perm-result"><Check size={13} aria-hidden="true" /> Answered</div> : null}
      </div>
    );
  }

  return (
    <form className="oa-perm oa-ask" aria-label="The agent has a question" onSubmit={(e) => { e.preventDefault(); submit(); }}>
      <div className="oa-perm-head"><ShieldCheck size={15} aria-hidden="true" /><span>OpenApp</span><em>the agent asks</em></div>
      {qs.map((q, qi) => {
        const d = drafts[q.id];
        const name = `${uid}-${qi}`;
        const hasOpts = !!q.options?.length;
        return (
          <fieldset key={q.id} className="oa-q">
            {q.header ? <legend className="oa-q-header">{q.header}</legend> : null}
            <div className="oa-q-text">{q.question}</div>
            {hasOpts ? (
              <div className="oa-q-opts" role="radiogroup" aria-label={q.question}>
                {q.options!.map((o) => (
                  <label key={o.label} className={`oa-q-opt${d?.choice === o.label ? " is-on" : ""}`}>
                    <input type="radio" name={name} checked={d?.choice === o.label} onChange={() => set(q.id, { choice: o.label })} />
                    <span className="oa-q-label">{o.label}</span>
                    {o.description ? <span className="oa-q-desc">{o.description}</span> : null}
                  </label>
                ))}
                {q.isOther ? (
                  <label className={`oa-q-opt${d?.choice === OTHER ? " is-on" : ""}`}>
                    <input type="radio" name={name} checked={d?.choice === OTHER} onChange={() => set(q.id, { choice: OTHER })} />
                    <span className="oa-q-label">Other</span>
                  </label>
                ) : null}
              </div>
            ) : null}
            {(!hasOpts || d?.choice === OTHER) ? (
              <input
                className="oa-input oa-q-input"
                type={q.isSecret ? "password" : "text"}
                autoComplete="off"
                aria-label={hasOpts ? `${q.question} (other)` : q.question}
                placeholder={hasOpts ? "Type your answer" : q.isSecret ? "Secret value" : "Your answer"}
                value={d?.text ?? ""}
                autoFocus={hasOpts}
                onChange={(e) => set(q.id, { text: e.target.value })}
              />
            ) : null}
          </fieldset>
        );
      })}
      <div className="oa-perm-actions">
        <button type="submit" className="oa-btn oa-btn-primary" disabled={!complete || busy}>Submit</button>
        <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => void send({})}>Cancel</button>
      </div>
    </form>
  );
}
