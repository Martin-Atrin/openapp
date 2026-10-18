import type { ReactNode } from "react";
import { GlowingEffect } from "@/components/ui/glowing-effect";
import { BentoGrid, BentoGridItem } from "@/components/ui/bento-grid";
import { cn } from "@/lib/utils";

function Side({ tone, title, children }: { tone: "mac" | "vm"; title: string; children: ReactNode }) {
  return (
    <div
      className={cn(
        "relative rounded-[var(--radius-win)] border-[1.5px] p-[26px]",
        tone === "mac" ? "border-mac bg-mac-soft" : "scope-inverse border-transparent",
      )}
    >
      <GlowingEffect tone={tone} spread={40} glow={false} disabled={false} proximity={72} inactiveZone={0.01} borderWidth={2} />
      <h3 className={cn("mb-[0.8em] text-[1.25rem]", tone === "vm" && "text-vm")}>{title}</h3>
      <ul className="relative pl-[1.1em]">{children}</ul>
    </div>
  );
}

const li = "mb-[0.6em] text-[0.96rem]";

function Arch() {
  return (
    <figure
      className="mx-0 mt-[2.2em] mb-[3.4em] grid grid-cols-1 items-stretch lg:grid-cols-[1fr_0.72fr_1fr]"
      aria-label="Diagram: your Mac on the left, the agent's VM on the right, connected by one bridge where every crossing has a consent rule."
    >
      <Side tone="mac" title="Your Mac">
        <li className={li}><strong>Shell</strong> — the window you see, rendering whatever the agent placed</li>
        <li className={li}><strong>Kernel</strong> (Rust) — layout, sessions, consent, secrets, the network proxy</li>
        <li className={li}><strong>Host services</strong> — microphone, on-device Speech and Apple Intelligence, Vision, files, notifications</li>
        <li className={li}><strong>Host modules</strong> — Swift the agent writes for anything else on macOS; you review the code first</li>
      </Side>
      <div className="relative z-[1] mx-[26px] -my-[2px] self-center border-x-2 border-dashed border-ink bg-surface px-[18px] py-[22px] text-center lg:mx-[-2px] lg:my-0 lg:border-x-0 lg:border-y-2">
        <h3>The bridge</h3>
        <p className="mx-auto mb-3 text-[0.92rem]">One authenticated channel. Every crossing has a consent rule.</p>
        <div className="flex flex-wrap justify-center gap-1.5">
          {[
            ["Free", "border-solid bg-ink text-paper border-ink"],
            ["Ask once", "border-solid bg-surface text-ink border-ink"],
            ["Ask every time", "border-dashed bg-surface text-ink border-ink"],
          ].map(([label, color]) => (
            <span key={label} className={cn("rounded-full border-[1.5px] px-[9px] py-[3px] text-[0.78rem] font-semibold", color)}>
              {label}
            </span>
          ))}
        </div>
      </div>
      <Side tone="vm" title="The agent's VM">
        <li className={li}><strong>The agent</strong> works here without its own permission prompts — the VM is the boundary</li>
        <li className={li}><strong>Module servers and daemons</strong> — any language, apt packages, even x86 tools</li>
        <li className={li}><strong>No network card</strong> — traffic leaves only through the kernel's proxy</li>
        <li className={li}><strong>/workspace</strong> — your app, a git repo that lives on your Mac</li>
      </Side>
    </figure>
  );
}

/* ---------- Bento headers: small token-drawn visuals ---------- */
const frame = "dot-grid relative flex h-full min-h-[6.5rem] w-full flex-1 overflow-hidden rounded-lg border border-line bg-paper";

function ViewsVisual() {
  return (
    <div className={cn(frame, "items-end justify-center gap-3 px-4 pt-4")}>
      <div className="h-[78%] w-[42%] rounded-t-lg border border-b-0 border-line bg-surface p-2">
        <div className="mb-1.5 flex gap-1"><i className="size-1.5 rounded-full bg-line" /><i className="size-1.5 rounded-full bg-line" /><i className="size-1.5 rounded-full bg-line" /></div>
        <div className="mb-1 h-1.5 w-3/4 rounded bg-mac-soft" />
        <div className="mb-1 h-1.5 w-1/2 rounded bg-mac-soft" />
        <div className="mt-2 h-6 rounded bg-vm-soft" />
      </div>
      <div className="relative h-[62%] w-[34%] rounded-t-lg border border-b-0 border-line bg-surface">
        <div className="absolute inset-y-0 right-0 w-[58%] border-l border-line bg-surface p-1.5 shadow-[-8px_0_16px_-12px_rgb(var(--shadow)/0.5)]">
          <div className="mb-1 h-1.5 w-3/4 rounded bg-vm" />
          <div className="mb-1 h-1.5 w-full rounded bg-mac-soft" />
          <div className="h-1.5 w-2/3 rounded bg-mac-soft" />
        </div>
      </div>
      <div className="absolute inset-x-0 top-0 flex h-4 items-center justify-end gap-1.5 border-b border-line bg-surface/80 px-2">
        <i className="size-1.5 rounded-full bg-vm" /><i className="h-1.5 w-4 rounded bg-line" />
      </div>
    </div>
  );
}

function ChannelVisual() {
  const bars = [6, 14, 22, 11, 28, 34, 18, 9, 24, 30, 16, 8, 20, 26, 12, 5];
  return (
    <div className={cn(frame, "items-center justify-center")}>
      <svg viewBox="0 0 160 60" className="h-16 w-[85%]" aria-hidden="true">
        {bars.map((h, i) => (
          <rect key={i} x={i * 10 + 2} y={30 - h / 2 - 2} width="5" height={h + 4} rx="2.5" fill="var(--vm)" opacity={0.35 + (h / 34) * 0.65} />
        ))}
      </svg>
    </div>
  );
}

function ServicesVisual() {
  return (
    <div className={cn(frame, "flex-col justify-center gap-1.5 px-3 text-[0.72rem]")}>
      {[["proton-bridge", "running"], ["libreoffice", "running"], ["dev server :5173", "running"]].map(([name, state]) => (
        <span key={name} className="flex items-center justify-between rounded-md border border-line bg-surface px-2 py-1">
          <span className="font-mono text-[0.68rem]">{name}</span>
          <span className="flex items-center gap-1 text-vm"><i className="size-1.5 rounded-full bg-vm" />{state}</span>
        </span>
      ))}
    </div>
  );
}

function FilesVisual() {
  return (
    <div className={cn(frame, "items-center justify-center gap-3 px-3")}>
      <svg viewBox="0 0 48 38" className="h-12 w-14 shrink-0" aria-hidden="true">
        <path d="M3 8a4 4 0 0 1 4-4h11l4 5h19a4 4 0 0 1 4 4v19a4 4 0 0 1-4 4H7a4 4 0 0 1-4-4z" fill="var(--mac-soft)" stroke="var(--mac)" strokeWidth="1.5" />
        <path d="M3 14h42" stroke="var(--mac)" strokeWidth="1.5" />
      </svg>
      <span className="grid gap-1 text-[0.7rem] leading-tight">
        <span className="font-mono">~/Documents</span>
        <span className="font-mono text-vm">→ /mnt/documents</span>
      </span>
    </div>
  );
}

function JobsVisual() {
  return (
    <div className={cn(frame, "flex-col justify-center gap-1.5 px-3 text-[0.72rem]")}>
      {[["Morning summary", "0 8 * * *"], ["Inbox sync", "*/15 * * * *"]].map(([name, cron]) => (
        <span key={name} className="flex items-center justify-between gap-2 rounded-md border border-line bg-surface px-2 py-1">
          <span className="font-medium">{name}</span>
          <span className="font-mono text-[0.66rem] text-ink-2">{cron}</span>
        </span>
      ))}
    </div>
  );
}

function AgentVisual() {
  return (
    <div className={cn(frame, "flex-col items-center justify-center gap-2 px-3 text-[0.72rem]")}>
      <span className="flex items-center gap-1.5 rounded-full border-[1.5px] border-vm bg-surface px-2.5 py-1 font-semibold text-vm">
        <i className="size-1.5 rounded-full bg-vm" />summary agent
      </span>
      <span className="flex gap-1">
        {["mail.read", "notify"].map((t) => (
          <span key={t} className="rounded-md border border-line bg-surface px-1.5 py-0.5 font-mono text-[0.64rem] text-ink-2">{t}</span>
        ))}
      </span>
    </div>
  );
}

function SecretVisual() {
  return (
    <div className={cn(frame, "items-center justify-center gap-2.5 px-3")}>
      <svg viewBox="0 0 24 24" className="size-7 shrink-0" aria-hidden="true">
        <circle cx="8" cy="12" r="4.5" fill="none" stroke="var(--mac)" strokeWidth="1.8" />
        <path d="M12.5 12H21M18 12v3M21 12v2.5" stroke="var(--mac)" strokeWidth="1.8" strokeLinecap="round" />
      </svg>
      <span className="grid gap-0.5 text-[0.7rem] leading-tight">
        <span className="font-mono">PROTON_TOKEN</span>
        <span className="tracking-[0.2em] text-ink-2">••••••••</span>
      </span>
    </div>
  );
}

function ShellVisual() {
  return (
    <div className={cn(frame, "flex-col")}>
      <div className="flex h-6 items-center justify-between border-b border-line bg-surface px-3 text-[0.68rem]">
        <span className="flex items-center gap-3">
          <b className="font-semibold">OpenApp</b>
          <span className="text-ink-2">File</span>
          <span className="text-ink-2">Edit</span>
          <span className="hidden text-ink-2 sm:inline">View</span>
          <span className="hidden text-ink-2 sm:inline">Window</span>
        </span>
        <span className="flex items-center gap-2.5">
          <span className="flex items-center gap-1 rounded bg-vm-soft px-1.5 py-px text-vm">
            <svg viewBox="0 0 24 24" className="size-2.5" aria-hidden="true"><path d="M9 18V6l10-2v12" fill="none" stroke="currentColor" strokeWidth="2.4" /><circle cx="7" cy="18" r="2.5" fill="currentColor" /><circle cx="17" cy="16" r="2.5" fill="currentColor" /></svg>
            Now playing
          </span>
          <i className="size-2 rounded-full bg-line" />
          <span className="text-ink-2">9:41</span>
        </span>
      </div>
      <div className="flex flex-1 items-start justify-end gap-2 p-3">
        <span className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[0.68rem] shadow-[0_10px_24px_-16px_rgb(var(--shadow)/0.5)]">
          <b className="block font-semibold">Morning summary</b>
          <span className="text-ink-2">3 new mails, 2 events today</span>
        </span>
        <span className="hidden rounded-md border border-line bg-surface px-2 py-1 font-mono text-[0.64rem] text-ink-2 sm:inline">⌘⇧Space</span>
      </div>
    </div>
  );
}

const blocks: { title: string; description: string; header: ReactNode; wide?: boolean }[] = [
  { title: "Views", description: "Native components, any HTML or real web pages — in sheets, windows or the menu bar", header: <ViewsVisual />, wide: true },
  { title: "Live channels", description: "Streams from backends to views: meters, terminals, new mail", header: <ChannelVisual /> },
  { title: "Services", description: "Daemons, terminals, Linux apps and local web apps shown in a view", header: <ServicesVisual /> },
  { title: "Files", description: "Open and save panels, folders you grant mounted into the VM", header: <FilesVisual /> },
  { title: "Jobs", description: "Schedules, file watches and webhooks that run in the background", header: <JobsVisual /> },
  { title: "Agents", description: "Small scoped agents inside modules, with only the tools you allow", header: <AgentVisual /> },
  { title: "Secrets", description: "Keychain storage, OAuth sign-in, requests signed on the Mac", header: <SecretVisual /> },
  { title: "Native shell", description: "Windows, menus, tray, shortcuts, notifications, deep links", header: <ShellVisual />, wide: true },
];

export function How() {
  return (
    <section id="how" aria-labelledby="how-title">
      <div className="wrap section rule">
        <h2 id="how-title">How it works</h2>
        <p className="section-lede">
          A small, fixed kernel and a lot of agent-written modules. The kernel never ships features — it ships the building
          blocks the agent assembles features from.
        </p>

        <Arch />

        <div className="grid grid-cols-1 gap-[clamp(28px,4vw,64px)] lg:grid-cols-2">
          <div className="min-w-0 lg:sticky lg:top-24 lg:self-start">
            <h3>A feature is a module</h3>
            <p>
              A folder with a manifest, its views and an optional backend. The smallest one needs no code at all — this is the
              whole dictation feature:
            </p>
            <pre>
              <code>
                <span className="k">"id"</span>: "dictation",{"\n"}
                <span className="k">"views"</span>: {'{ "mic": { "kind": "spec", "file": "views/mic.json" } }'},{"\n"}
                <span className="k">"placements"</span>: {'[{ "view": "mic", "anchor": "composer.actions" }]'}
              </code>
            </pre>
            <pre>
              <code>
                {"{ "}
                <span className="k">"type"</span>: "icon-button", <span className="k">"icon"</span>: "mic",{"\n  "}
                <span className="k">"action"</span>: {'{ "host": "audio.record", "transcribe": "apple", "insert": true } }'}
              </code>
            </pre>
            <p>
              The recording stays on your Mac and is transcribed on-device. When a feature needs state or an outside service,
              the agent adds a backend — an MCP server in any language — inside the VM.
            </p>
          </div>
          <div className="min-w-0">
            <h3>The building blocks</h3>
            <BentoGrid className="mt-3 md:auto-rows-[minmax(15rem,auto)] md:grid-cols-2">
              {blocks.map((b) => (
                <BentoGridItem
                  key={b.title}
                  title={b.title}
                  description={b.description}
                  header={b.header}
                  className={b.wide ? "md:col-span-2" : undefined}
                />
              ))}
            </BentoGrid>
          </div>
        </div>
      </div>
    </section>
  );
}
