// §20.1 secret card (kernel chrome): masked input whose value goes straight to the Keychain via submit_secret.
// The value lives only in this component's state until it is sent, and is cleared right after.
import { useId, useState } from "react";
import { Check, Copy, ExternalLink, Eye, EyeOff, KeyRound, X } from "lucide-react";
import type { ChatItem } from "./contract";
import { kernel } from "./kernel";
import { useShell } from "./store";

const hostOf = (url: string) => {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};

export function SecretCard({ item }: { item: ChatItem }) {
  const { toast, runAction } = useShell();
  const uid = useId();
  const [value, setValue] = useState("");
  const [show, setShow] = useState(false);
  const [busy, setBusy] = useState(false);
  const resolved = item.resolved !== undefined;
  const label = item.title || item.name || "Secret";

  const send = async (v: string | null) => {
    setBusy(true);
    try {
      await kernel.submitSecret(item.id, v);
      setValue("");
    } catch (err) {
      toast(String(err), "error");
    } finally {
      setBusy(false);
    }
  };

  if (resolved) {
    return (
      <div className="oa-perm is-resolved oa-secret" role="group" aria-label={`Secret: ${label}`}>
        <div className="oa-perm-head"><KeyRound size={15} aria-hidden="true" /><span>OpenApp</span><em>secret</em></div>
        <div className="oa-perm-title">{label}</div>
        <div className="oa-perm-result">
          {item.resolved === "saved" ? <><Check size={13} aria-hidden="true" /> Saved to your Keychain</> : <><X size={13} aria-hidden="true" /> Cancelled</>}
        </div>
      </div>
    );
  }

  const isClient = item.secretKind === "oauth-client";
  return (
    <form
      className="oa-perm oa-secret"
      aria-label={`OpenApp asks for ${label}`}
      autoComplete="off"
      onSubmit={(e) => {
        e.preventDefault();
        if (value.trim() && !busy) void send(value);
      }}
    >
      <div className="oa-perm-head"><KeyRound size={15} aria-hidden="true" /><span>OpenApp</span><em>asks for a secret</em></div>
      <label className="oa-perm-title" htmlFor={uid}>{label}</label>
      {item.text ? <div className="oa-perm-text">{item.text}</div> : null}
      {item.url ? (
        <button type="button" className="oa-btn oa-btn-ghost oa-secret-link" onClick={() => void runAction("chat", { host: "open_url", url: item.url })}>
          <ExternalLink size={13} aria-hidden="true" /> Open {hostOf(item.url)}
        </button>
      ) : null}
      <div className="oa-secret-row">
        <input
          id={uid}
          className="oa-input oa-secret-input"
          type={show ? "text" : "password"}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          data-1p-ignore
          placeholder={isClient ? "Paste the client JSON or client ID" : item.secretKind === "password" ? "Password" : "Paste the token"}
          value={value}
          autoFocus
          onChange={(e) => setValue(e.target.value)}
        />
        <button type="button" className="oa-iconbtn" aria-label={show ? "Hide" : "Show"} title={show ? "Hide" : "Show"} onClick={() => setShow((s) => !s)}>
          {show ? <EyeOff size={15} aria-hidden="true" /> : <Eye size={15} aria-hidden="true" />}
        </button>
      </div>
      <div className="oa-secret-note">
        Saved in your Mac's Keychain. {item.requestedBy ? `${item.requestedBy} gets only its name, never the value.` : "The app gets only its name, never the value."}
        {item.hosts?.length ? ` Used only with ${item.hosts.join(", ")}.` : ""}
      </div>
      <div className="oa-perm-actions">
        <button type="submit" className="oa-btn oa-btn-primary" disabled={!value.trim() || busy}>Save</button>
        <button type="button" className="oa-btn oa-btn-ghost" disabled={busy} onClick={() => void send(null)}>Cancel</button>
      </div>
    </form>
  );
}

/** A kernel system line with a link (OAuth sign-in) and/or a device code. */
export function SystemLink({ item }: { item: ChatItem }) {
  const { runAction, toast } = useShell();
  return (
    <div className="oa-line oa-line-link">
      <span>{item.text}</span>
      {item.code ? (
        <button
          type="button"
          className="oa-code"
          title="Copy the code"
          onClick={() => void navigator.clipboard?.writeText(item.code ?? "").then(() => toast("Code copied"), () => {})}
        >
          {item.code} <Copy size={12} aria-hidden="true" />
        </button>
      ) : null}
      {item.url ? (
        <button type="button" className="oa-btn oa-btn-ghost oa-secret-link" onClick={() => void runAction("chat", { host: "open_url", url: item.url })}>
          <ExternalLink size={13} aria-hidden="true" /> {item.linkLabel || "Open"}
        </button>
      ) : null}
    </div>
  );
}
