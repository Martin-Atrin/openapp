import { useState } from "react";
import type { Region as RegionT } from "./contract";
import { PlacementView } from "./PlacementView";
import { useShell } from "./store";

export function Region({ region }: { region: RegionT }) {
  const { placementsAt } = useShell();
  const list = placementsAt(`region:${region.id}`);
  const [active, setActive] = useState<string | null>(null);
  const horizontal = region.edge === "left" || region.edge === "right";
  const size = Math.max(40, region.size || 280);
  const current = list.find((p) => p.id === active) ?? list[0];
  const style = horizontal ? { width: size } : { height: size };
  const inner = horizontal ? { width: size } : { height: size };

  return (
    <aside className={`oa-region oa-region-${region.edge}`} style={style} data-region={region.id} aria-label={region.title || region.id}>
      <div className="oa-region-inner" style={inner}>
        {region.title ? <div className="oa-region-title">{region.title}</div> : null}
        {region.mode === "tabs" && list.length > 0 ? (
          <>
            <div className="oa-tabs" role="tablist" aria-label={region.title || region.id}>
              {list.map((p) => {
                const sel = p.id === current?.id;
                return (
                  <button
                    key={p.id}
                    type="button"
                    role="tab"
                    aria-selected={sel}
                    className={`oa-tab${sel ? " is-active" : ""}`}
                    onClick={() => setActive(p.id)}
                  >
                    {p.label || p.view}
                  </button>
                );
              })}
            </div>
            <div className="oa-region-body" role="tabpanel">
              {current ? <PlacementView key={current.id} placement={current} /> : null}
            </div>
          </>
        ) : (
          <div className="oa-region-body oa-region-stack">
            {list.map((p) => (
              <PlacementView key={p.id} placement={p} />
            ))}
          </div>
        )}
      </div>
    </aside>
  );
}
