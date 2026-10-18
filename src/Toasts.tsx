import { AlertCircle, Info, X } from "lucide-react";
import { useShell } from "./store";

export function Toasts() {
  const { toasts, dismissToast } = useShell();
  return (
    <div className="oa-toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={`oa-toast oa-toast-${t.tone}`} role={t.tone === "error" ? "alert" : "status"}>
          {t.tone === "error" ? <AlertCircle size={15} aria-hidden="true" /> : <Info size={15} aria-hidden="true" />}
          <span className="oa-toast-text">{t.text}</span>
          <button type="button" className="oa-toast-x" aria-label="Dismiss" onClick={() => dismissToast(t.id)}><X size={13} aria-hidden="true" /></button>
        </div>
      ))}
    </div>
  );
}
