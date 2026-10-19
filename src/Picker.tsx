import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { AlertCircle, Bot, KeyRound, Loader2 } from "lucide-react";
import type { HarnessInfo } from "./contract";
import { kernel } from "./kernel";
import { useShell } from "./store";

type Mode = "pick" | "starting" | "error" | "signin" | "account";

/** Window event that opens the account card (the model chip sends it; `App` listens). */
export const ACCOUNTS_EVENT = "oa:accounts";
export function openAccounts() {
  window.dispatchEvent(new Event(ACCOUNTS_EVENT));
}

/** Pi: paste the address the browser ended on (browser on another computer, or a blocked callback). */
function PasteUrl({ busy, onSubmit }: { busy: boolean; onSubmit: (v: string) => void }) {
  const [value, setValue] = useState("");
  return (
    <form className="oa-signin-paste" onSubmit={(e) => { e.preventDefault(); if (value.trim()) onSubmit(value.trim()); }}>
      <label className="oa-signin-note" htmlFor="oa-signin-paste">If the page does not finish, paste the address it ended on:</label>
      <div className="oa-secret-row">
        <input id="oa-signin-paste" className="oa-input oa-secret-input" type="text" autoComplete="off" autoCorrect="off" autoCapitalize="off" spellCheck={false} data-1p-ignore placeholder="http://127.0.0.1:…/callback?code=…" value={value} onChange={(e) => setValue(e.target.value)} />
        <button type="submit" className="oa-btn" disabled={busy || !value.trim()}>Continue</button>
      </div>
    </form>
  );
}

/** Pi: an API key goes into the VM's own login file (never into chat or logs). */
function ApiKeyForm({ providers, busy, onSubmit }: { providers: { id: string; label: string }[]; busy: boolean; onSubmit: (provider: string, key: string) => void }) {
  const [provider, setProvider] = useState(providers[0]?.id ?? "");
  const [key, setKey] = useState("");
  return (
    <form className="oa-signin-paste" onSubmit={(e) => { e.preventDefault(); if (provider && key.trim()) { onSubmit(provider, key.trim()); setKey(""); } }}>
      <div className="oa-signin-actions" role="radiogroup" aria-label="API key provider">
        {providers.map((p) => (
          <button key={p.id} type="button" role="radio" aria-checked={provider === p.id} className={`oa-btn${provider === p.id ? " oa-btn-primary" : " oa-btn-ghost"}`} onClick={() => setProvider(p.id)}>{p.label}</button>
        ))}
      </div>
      <div className="oa-secret-row">
        <input className="oa-input oa-secret-input" type="password" autoComplete="off" autoCorrect="off" autoCapitalize="off" spellCheck={false} data-1p-ignore aria-label="API key" placeholder="API key" value={key} onChange={(e) => setKey(e.target.value)} />
        <button type="submit" className="oa-btn oa-btn-primary" disabled={busy || !key.trim() || !provider}>Save</button>
      </div>
    </form>
  );
}

/** One stored login on the account card: the name, "in use", and a two-step Sign out. */
function LoginRow({ label, active, busy, onSignOut }: { label: string; active: boolean; busy: boolean; onSignOut: () => void }) {
  const [confirm, setConfirm] = useState(false);
  return (
    <li className="oa-account-row">
      <span className="oa-account-name">{label}</span>
      {active ? <span className="oa-model-tag is-muted">in use</span> : null}
      {confirm ? (
        <span className="oa-account-confirm">
          <button type="button" className="oa-btn oa-btn-danger" disabled={busy} onClick={onSignOut}>Sign out</button>
          <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => setConfirm(false)}>Keep</button>
        </span>
      ) : (
        <button type="button" className="oa-btn oa-btn-ghost oa-account-out" disabled={busy} onClick={() => setConfirm(true)}>Sign out…</button>
      )}
    </li>
  );
}

/** Pi sign-in choices that match a stored login get "again" ("Sign in again with ChatGPT"). */
const METHOD_LOGIN: Record<string, { provider: string; type: string }> = { chatgpt: { provider: "openai", type: "oauth" }, claude: { provider: "anthropic", type: "oauth" } };

/**
 * §17: sign-in card for a VM provider ("Sign in to Codex for OpenApp"). With `onClose` it is the account card,
 * opened from the model chip while signed in: the stored logins (sign out), sign in again, another provider, or
 * a new API key (it replaces the provider's old key). Both use the same sign-in flows.
 */
function SignIn({ h, empty, onReopen, onClose }: { h: HarnessInfo; empty: boolean; onReopen: () => void; onClose?: () => void }) {
  const { toast } = useShell();
  const [busy, setBusy] = useState(false);
  const [showKey, setShowKey] = useState(false);
  const manage = !!onClose;
  const login = h.login ?? { state: "required" as const };
  const offered = login.methods ?? h.accounts?.methods;
  const methods = offered?.length ? offered : null;
  const logins = h.accounts?.logins ?? [];
  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    try {
      await f();
    } catch (err) {
      toast(String(err), "error");
    } finally {
      setBusy(false);
    }
  };
  const start = (method?: string) => run(() => kernel.harnessLogin(h.id, method));
  const submit = (kind: "code" | "apikey", value: string, provider?: string) => run(() => kernel.harnessLoginSubmit(h.id, kind, value, provider));
  const signOut = (provider: string) => run(() => kernel.harnessLogout(h.id, provider));
  const openUrl = (url: string) => void kernel.uiAction("chat", { host: "open_url", url });
  const product = h.name.split(" · ")[0] || h.name;
  const waiting = login.state === "starting" || busy;
  const keyMethod = methods?.find((m) => m.providers?.length);
  const methodLabel = (id: string, label: string) => {
    const l = METHOD_LOGIN[id];
    return l && logins.some((x) => x.provider === l.provider && x.type === l.type) ? label.replace(/^Sign in/, "Sign in again") : label;
  };
  return (
    <div className="oa-composer-block oa-picker-block" data-empty={empty}>
      <div className="oa-picker oa-signin" aria-busy={waiting}>
        <h1 className="oa-picker-title">{manage ? `Accounts for ${product}` : `Sign in to ${product} for OpenApp`}</h1>
        <p className="oa-picker-hint">{product} runs in OpenApp's isolated VM with its own login. Your login stays inside the VM.</p>
        <div className="oa-signin-body">
          {login.state === "device" ? (
            <>
              <p className="oa-signin-step">Open the sign-in page and enter this code:</p>
              <p className="oa-signin-code" aria-label="Sign-in code">{login.code}</p>
              {login.url ? <button type="button" className="oa-btn oa-btn-primary" onClick={() => openUrl(login.url!)}>Open sign-in page</button> : null}
            </>
          ) : login.state === "browser" ? (
            <>
              <p className="oa-signin-step"><Loader2 size={14} className="oa-spin" aria-hidden="true" /> Finish signing in in your browser…</p>
              <div className="oa-signin-actions">
                {login.url ? <button type="button" className="oa-btn" onClick={() => openUrl(login.url!)}>Open the page again</button> : null}
                {methods || login.paste ? (
                  <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => start("cancel")}>Cancel</button>
                ) : (
                  <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => start("device")}>Use a code instead</button>
                )}
              </div>
              {login.paste ? <PasteUrl busy={busy} onSubmit={(v) => void submit("code", v)} /> : null}
            </>
          ) : waiting ? (
            <p className="oa-signin-step"><Loader2 size={14} className="oa-spin" aria-hidden="true" /> {busy && manage ? "Working…" : "Starting sign-in…"}</p>
          ) : methods ? (
            <>
              {login.state === "failed" ? <p className="oa-signin-error" role="alert">{login.message || "Sign-in failed."}</p> : null}
              {manage && logins.length ? (
                <ul className="oa-account-list" aria-label="Stored logins">
                  {logins.map((l) => <LoginRow key={l.provider} label={l.label} active={l.active} busy={busy} onSignOut={() => void signOut(l.provider)} />)}
                </ul>
              ) : null}
              {manage ? <p className="oa-signin-note">{logins.length ? "Sign in again, add another provider, or replace an API key:" : "Sign in:"}</p> : null}
              <div className="oa-signin-actions">
                {methods.filter((m) => !m.providers?.length).map((m, i) => (
                  <button key={m.id} type="button" className={`oa-btn${i === 0 ? " oa-btn-primary" : ""}`} onClick={() => start(m.id)}>
                    {i === 0 ? <KeyRound size={14} aria-hidden="true" /> : null} {methodLabel(m.id, m.label)}
                  </button>
                ))}
                {keyMethod ? (
                  <button type="button" className="oa-btn oa-btn-ghost" aria-expanded={showKey} onClick={() => setShowKey((v) => !v)}>{keyMethod.label}</button>
                ) : null}
              </div>
              {methods.filter((m) => m.note).map((m) => <p key={m.id} className="oa-signin-note">{m.label}: {m.note}</p>)}
              {keyMethod && showKey ? <ApiKeyForm providers={keyMethod.providers!} busy={busy} onSubmit={(p, k) => void submit("apikey", k, p)} /> : null}
            </>
          ) : (
            <>
              {login.state === "failed" ? <p className="oa-signin-error" role="alert">{login.message || "Sign-in failed."}</p> : null}
              <div className="oa-signin-actions">
                <button type="button" className="oa-btn oa-btn-primary" onClick={() => start()}><KeyRound size={14} aria-hidden="true" /> Sign in with ChatGPT</button>
                <button type="button" className="oa-btn oa-btn-ghost" onClick={() => start("device")}>Use a code instead</button>
              </div>
            </>
          )}
        </div>
        <div className="oa-signin-actions oa-signin-other">
          {manage ? <button type="button" className="oa-btn" onClick={onClose}>Done</button> : null}
          <button type="button" className="oa-btn oa-btn-ghost" onClick={onReopen}>Pick another provider</button>
        </div>
      </div>
    </div>
  );
}

function indicator(h: HarnessInfo): { tone: "ok" | "unknown" | "off"; label: string } {
  if (h.comingSoon) return { tone: "off", label: "Coming soon" };
  if (!h.available) return { tone: "off", label: "Unavailable" };
  if (h.signedIn === true) return { tone: "ok", label: "Signed in" };
  if (h.signedIn === false) return { tone: "unknown", label: "Not signed in" };
  return { tone: "unknown", label: "Available" };
}

/** Kernel chrome: shown instead of the composer until a provider is picked (§11). */
export function Picker({ mode, empty, onReopen, onCloseAccount }: { mode: Mode; empty: boolean; onReopen: () => void; onCloseAccount?: () => void }) {
  const { snapshot, toast } = useShell();
  const { agent } = snapshot;
  // Release gating: usable providers first (stable), "Coming soon" ones after.
  const harnesses = [...(snapshot.harnesses ?? [])].sort((a, b) => Number(!!a.comingSoon) - Number(!!b.comingSoon));
  const gated = harnesses.some((h) => h.comingSoon);
  const [choosing, setChoosing] = useState<string | null>(null);
  const grid = useRef<HTMLDivElement>(null);
  const starting = mode === "starting" || choosing !== null;
  const startingId = choosing ?? agent.harness ?? null;

  // Once the kernel reports anything other than unselected/error, the optimistic choice is over.
  useEffect(() => {
    if (agent.status !== "unselected" && agent.status !== "error") setChoosing(null);
  }, [agent.status]);

  // Focus the first available card when the grid appears.
  useEffect(() => {
    if (mode === "pick") grid.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  }, [mode]);

  const pick = async (h: HarnessInfo) => {
    if (!h.available || h.comingSoon || starting) return;
    setChoosing(h.id);
    try {
      await kernel.selectHarness(h.id);
    } catch (err) {
      setChoosing(null);
      toast(String(err), "error");
    }
  };

  const onKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const keys = ["ArrowRight", "ArrowDown", "ArrowLeft", "ArrowUp"];
    if (!keys.includes(e.key)) return;
    const btns = [...(grid.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
    const i = btns.indexOf(document.activeElement as HTMLButtonElement);
    if (i < 0 || !btns.length) return;
    e.preventDefault();
    const next = e.key === "ArrowRight" || e.key === "ArrowDown" ? (i + 1) % btns.length : (i - 1 + btns.length) % btns.length;
    btns[next]!.focus();
  };

  if (mode === "signin" || mode === "account") {
    const h = harnesses.find((x) => x.id === agent.harness);
    if (h) return <SignIn h={h} empty={empty} onReopen={onReopen} onClose={mode === "account" ? onCloseAccount : undefined} />;
  }

  if (mode === "error") {
    return (
      <div className="oa-composer-block oa-picker-block" data-empty={empty}>
        <div className="oa-picker oa-picker-error" role="alert">
          <div className="oa-picker-errhead"><AlertCircle size={16} aria-hidden="true" /> The provider could not start</div>
          <p className="oa-picker-errmsg">{agent.error || "Unknown error."}</p>
          <button type="button" className="oa-btn oa-btn-primary" onClick={onReopen}>Pick another provider</button>
        </div>
      </div>
    );
  }

  return (
    <div className="oa-composer-block oa-picker-block" data-empty={empty}>
      <div className="oa-picker" aria-busy={starting}>
        <h1 className="oa-picker-title">Pick your provider</h1>
        <p className="oa-picker-hint">{gated ? "OpenApp's agent runs in its own private VM on your Mac. Sign in with ChatGPT, Claude or an API key." : "OpenApp builds itself with the coding agent you already use."}</p>
        <div className="oa-picker-grid" role="group" aria-label="Providers" ref={grid} onKeyDown={onKey}>
          {harnesses.map((h) => {
            const ind = indicator(h);
            const isStarting = starting && startingId === h.id;
            const soon = !!h.comingSoon;
            const disabled = !h.available || soon || starting;
            return (
              <button
                key={h.id}
                type="button"
                className={`oa-pcard${!h.available && !soon ? " is-unavailable" : ""}${soon ? " is-soon" : ""}${!soon && gated ? " is-primary" : ""}${isStarting ? " is-starting" : ""}`}
                disabled={disabled}
                aria-disabled={disabled}
                onClick={() => pick(h)}
              >
                <span className="oa-pcard-top">
                  <Bot size={16} aria-hidden="true" />
                  <span className="oa-pcard-name">{h.name}</span>
                  {soon ? <span className="oa-pcard-badge">Coming soon</span> : null}
                  {isStarting ? <Loader2 size={14} className="oa-spin oa-pcard-spin" aria-label="Starting" /> : null}
                </span>
                <span className="oa-pcard-note">{isStarting ? "Starting…" : soon ? "" : h.note || ind.label}</span>
                {soon ? null : <span className={`oa-pcard-ind oa-pcard-ind-${ind.tone}`}><span className="oa-pcard-dot" aria-hidden="true" />{ind.label}</span>}
              </button>
            );
          })}
          {harnesses.length === 0 ? <p className="oa-picker-hint">No providers detected yet…</p> : null}
        </div>
      </div>
    </div>
  );
}
