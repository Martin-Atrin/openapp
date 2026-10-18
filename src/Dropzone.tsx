// §21 G4 spec node `dropzone { label?, hint?, icon?, action, children? }`: native file drops (copied into the
// module's data/inbox or passed from a granted folder) run `action` with `{ files: [{ path, name, size, mime }] }`.
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Icon } from "./Icon";
import { usePlacement } from "./PlacementView";
import { useShell } from "./store";
import type { FileRef } from "./shell";

export function SpecDropzone({ node, children }: { node: Record<string, unknown>; children?: ReactNode }) {
  const { runAction } = useShell();
  const { placementId } = usePlacement();
  const ref = useRef<HTMLDivElement>(null);
  const [last, setLast] = useState<string | null>(null);
  const action = node.action;
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onDrop = (e: Event) => {
      const files = ((e as CustomEvent<FileRef[]>).detail ?? []).filter((f) => !f.error);
      if (!files.length) return;
      setLast(files.length === 1 ? (files[0]!.name ?? "1 file") : `${files.length} files`);
      void runAction(placementId, action, { files });
    };
    el.addEventListener("oa-drop", onDrop);
    return () => el.removeEventListener("oa-drop", onDrop);
  }, [placementId, action, runAction]);
  const label = typeof node.label === "string" ? node.label : "Drop files here";
  const hint = typeof node.hint === "string" ? node.hint : last ? `Received ${last}` : undefined;
  return (
    <div ref={ref} className="oa-dropzone" data-dropzone="" role="region" aria-label={label}>
      {children ?? (
        <>
          <Icon name={typeof node.icon === "string" ? node.icon : "upload"} size={18} />
          <div className="oa-dropzone-label">{label}</div>
          {hint ? <div className="oa-dropzone-hint">{hint}</div> : null}
        </>
      )}
    </div>
  );
}
