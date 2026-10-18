import { HoverEffect, type HoverEffectItem } from "@/components/ui/card-hover-effect";
import { BackgroundBeams } from "@/components/ui/background-beams";
import { HoverBorderGradient } from "@/components/ui/hover-border-gradient";
import { cn } from "@/lib/utils";
import { GITHUB, Mark } from "./Top";

const ledger: [request: string, built: string, status: string | null][] = [
  ["A Proton Mail client", "Proton Mail Bridge as a VM service, a sign-in terminal, an inbox view", null],
  ["A music studio like GarageBand", "Tracks, microphone recording, a metronome, a keyboard instrument, export to Music", null],
  ["A terminal, editor and preview", "A real shell in a view, a code editor, a preview of local web servers", null],
  ["LibreOffice inside the app", "LibreOffice running in the VM, shown in a view", null],
  ["A unified inbox", "IMAP connectors with Keychain passwords and live updates", null],
  ["My Telegram, as me", "A user-account client as a service, phone sign-in, chats", null],
  ["My photo library", "A Photos grid with crop and export", null],
  ["A file manager and Markdown editor", "A Documents browser and an editor that opens .md files from Finder", null],
  ["A WebGPU game for a gamepad", "A full-screen game with gamepad input", null],
  ["Home Assistant and Bluetooth sensors", "A lights panel and a Bluetooth scanner", null],
  ["A morning summary at 8", "A daily job, a scoped agent, a notification", "Needed a sign-in"],
  ["A music player in the menu bar", "A player and a tray item", "Waited for a macOS permission"],
];

function Status({ partial, children }: { partial: boolean; children: string }) {
  return (
    <span className={cn("inline-flex items-center gap-2 text-[0.88rem] font-semibold", partial ? "text-bridge" : "text-vm")}>
      <i className="size-2 rounded-full bg-current" aria-hidden="true" />
      {children}
    </span>
  );
}

export function Proof() {
  const items: HoverEffectItem[] = ledger.map(([request, built, partial]) => ({
    key: request,
    title: <q>{request}</q>,
    description: built,
    footer: <Status partial={partial !== null}>{partial ?? "Built"}</Status>,
  }));
  return (
    <section id="proof" aria-labelledby="proof-title">
      <div className="wrap section rule">
        <h2 id="proof-title">What the agent has built</h2>
        <p className="section-lede">
          We test OpenApp by asking its agent for twelve real apps, one request each, in fresh workspaces and with nobody
          watching. In the latest run it built ten of them up to the steps only a person can do, like signing in or
          approving access.
        </p>
        <HoverEffect items={items} className="-mx-1.5 mt-[1.6em] mb-[1.2em]" />
        <p className="fine">
          Probe runs used Pi with a GPT model in build mode. Results and transcripts are in{" "}
          <a href={`${GITHUB}/tree/main/eval/probes`}>eval/probes</a>.
        </p>
      </div>
    </section>
  );
}

export function Safety() {
  return (
    <section aria-labelledby="safety-title">
      <div className="wrap section rule">
        <h2 id="safety-title">What OpenApp promises, and what it doesn't yet</h2>
        <div className="mt-[1.4em] grid grid-cols-1 gap-[clamp(20px,3vw,40px)] sm:grid-cols-2">
          <div className="rounded-[var(--radius-win)] bg-vm-soft p-[26px]">
            <h3 className="text-vm">Promised today</h3>
            <ul className="mb-[1em] pl-[1.1em] [&>li]:mb-[0.6em]">
              <li>The agent and its code run in a VM with no network card. Every connection goes through the kernel's proxy and is logged on your Mac.</li>
              <li>New sites, your microphone, camera, folders, clipboard and macOS data need your approval. Host modules show their source changes before they run.</li>
              <li>Passwords and tokens live in your Keychain. The agent only ever sees their names.</li>
              <li>The record of what was granted and built lives outside the folder the VM can see.</li>
            </ul>
          </div>
          <div className="rounded-[var(--radius-win)] bg-bridge-soft p-[26px]">
            <h3 className="text-bridge">Not yet</h3>
            <ul className="mb-[1em] pl-[1.1em] [&>li]:mb-[0.6em]">
              <li>Modules inside the VM are not isolated from each other.</li>
              <li>Web content is not yet labelled as untrusted when the agent reads it.</li>
              <li>Builds are not signed or notarized, so macOS asks you to confirm the first launch.</li>
            </ul>
            <p className="m-0">
              The full list is in the <a href={`${GITHUB}/blob/main/spec/ROADMAP.md`}>roadmap</a>.
            </p>
          </div>
        </div>
      </div>
    </section>
  );
}

export function Get() {
  return (
    <section id="get" className="relative isolate overflow-hidden" aria-labelledby="get-title">
      <BackgroundBeams className="-z-10 opacity-80" />
      <div className="wrap section rule relative">
        <h2 id="get-title">Get OpenApp</h2>
        <div className="mt-[1em] grid grid-cols-1 gap-[clamp(24px,4vw,64px)] sm:grid-cols-2">
          <div className="min-w-0">
            <h3>Download</h3>
            <p>
              Get <strong>OpenApp.app</strong> from the <a href={`${GITHUB}/releases/latest`}>latest release</a>, move it to
              Applications and open it. On first start it prepares its VM, which takes a few minutes and about 2&nbsp;GB of
              disk.
            </p>
            <p className="fine">
              Needs an Apple silicon Mac with macOS 26 or later, and a ChatGPT or Claude account or an OpenAI or Anthropic API
              key.
            </p>
            <HoverBorderGradient as="a" href={`${GITHUB}/releases/latest`} containerClassName="mt-1.5">
              Download the latest release
            </HoverBorderGradient>
          </div>
          <div className="min-w-0">
            <h3>Build from source</h3>
            <pre>
              <code>{"pnpm install\nnode scripts/fetch-runtime.ts\npnpm tauri build --bundles app"}</code>
            </pre>
            <p className="fine">
              Needs Rust (pinned in the repo), Node 24+, pnpm and Xcode 26. Run the tests with <code>pnpm test</code>.
            </p>
          </div>
        </div>
      </div>
    </section>
  );
}

export function Footer() {
  return (
    <footer className="wrap rule pt-8 pb-12">
      <div className="mb-3.5 flex flex-wrap items-center justify-between gap-4">
        <a className="wordmark" href="./">
          <Mark size={20} />
          OpenApp
        </a>
        <nav aria-label="Project" className="flex flex-wrap gap-5">
          {[
            ["Source", GITHUB],
            ["Contract", `${GITHUB}/blob/main/spec/CONTRACT.md`],
            ["Roadmap", `${GITHUB}/blob/main/spec/ROADMAP.md`],
            ["Notices", `${GITHUB}/blob/main/THIRD-PARTY-NOTICES.md`],
          ].map(([label, href]) => (
            <a key={label} href={href} className="text-[0.92rem] text-ink-2 no-underline hover:text-ink">
              {label}
            </a>
          ))}
        </nav>
      </div>
      <p className="m-0 max-w-[90ch] text-[0.85rem] text-ink-2">
        Open source under the Apache License 2.0. Bundles microsandbox and a Linux kernel under GPL-2.0 — see the notices.
        Typeset in Geologica (SIL Open Font License).
      </p>
    </footer>
  );
}
