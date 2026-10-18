import { createContext, useContext, useEffect } from "react";
import type { Placement } from "./contract";
import { HtmlView } from "./HtmlView";
import { SpecRenderer } from "./SpecRenderer";
import { WebView } from "./WebView";
import { useShell } from "./store";

interface PlacementCtx {
  placementId: string;
  depth: number;
}
export const PlacementContext = createContext<PlacementCtx>({ placementId: "shell", depth: 0 });
export const usePlacement = () => useContext(PlacementContext);

const MAX_DEPTH = 8;

export function ErrorBox({ children, placementId, report }: { children: string; placementId?: string; report?: boolean }) {
  const { reportError } = useShell();
  useEffect(() => {
    if (report && placementId) reportError(placementId, children);
  }, [report, placementId, children, reportError]);
  return (
    <div className="oa-errorbox" role="alert">
      {children}
    </div>
  );
}

/** Resolve a placement to its view and render it. */
export function PlacementView({ placement }: { placement: Placement }) {
  const { snapshot } = useShell();
  const { depth } = usePlacement();
  const key = `${placement.module}/${placement.view}`;
  const view = snapshot.views[key];

  let body;
  if (depth >= MAX_DEPTH) {
    body = <ErrorBox placementId={placement.id} report>{`Slot nesting too deep at ${key}`}</ErrorBox>;
  } else if (!view) {
    body = <ErrorBox placementId={placement.id} report>{`View not found: ${key}`}</ErrorBox>;
  } else if (view.kind === "error") {
    body = <ErrorBox>{`${key}: ${view.error}`}</ErrorBox>;
  } else if (view.kind === "html") {
    body = <HtmlView placement={placement} url={view.url} rev={view.rev} />;
  } else if (view.kind === "web") {
    body = <WebView placement={placement} url={view.url} />;
  } else {
    body = <SpecRenderer tree={view.tree} />;
  }
  return (
    <PlacementContext.Provider value={{ placementId: placement.id, depth: depth + 1 }}>
      <div className="oa-placement" data-placement={placement.id} data-view={key}>
        {body}
      </div>
    </PlacementContext.Provider>
  );
}

/** All placements at an anchor, ordered. Renders nothing if empty. */
export function Anchor({ name, className }: { name: string; className?: string }) {
  const { placementsAt } = useShell();
  const list = placementsAt(name);
  if (!list.length) return null;
  return (
    <div className={className} data-anchor={name}>
      {list.map((p) => (
        <PlacementView key={p.id} placement={p} />
      ))}
    </div>
  );
}
