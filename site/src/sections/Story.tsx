import type { ReactNode } from "react";
import { TextGenerateEffect } from "@/components/ui/text-generate-effect";
import { Timeline, type TimelineEntry } from "@/components/ui/timeline";

export function Why() {
  return (
    <section id="why" aria-labelledby="why-title">
      <div className="wrap section rule">
        <h2
          id="why-title"
          className="mb-[0.7em] max-w-[16ch] text-[clamp(2.3rem,1.1rem+4.6vw,4.6rem)] leading-none font-[720] tracking-[-0.035em]"
        >
          <TextGenerateEffect words="The only limit is what you can describe." duration={0.6} />
        </h2>
        <div className="grid grid-cols-1 gap-[clamp(24px,4vw,64px)] sm:grid-cols-2">
          <p className="text-[1.12rem] text-ink-2">
            Every AI app ships the features its makers picked. You wait, or you build something yourself and watch it
            turn up in the next release of someone else's product. Your day ends up spread across five apps that each do
            eighty percent of what you need.
          </p>
          <p className="text-[1.12rem]">
            OpenApp starts with nothing but a chat box. Ask for a dictation button, an inbox for your work mail, a music
            studio, a cat that sleeps on your sidebar — the agent builds it into the app, live. Next week you ask for
            something else. It stays yours: a folder of plain files on your Mac, versioned with git.
          </p>
        </div>
      </div>
    </section>
  );
}

const mini = "dot-grid flex h-[132px] flex-col justify-center gap-2 rounded-xl border border-line bg-surface p-3.5 text-[0.8rem] mb-[18px] max-w-[30rem]";
const row = "flex justify-between gap-2 rounded-[9px] border border-line bg-surface px-[11px] py-[9px]";

const steps: { n: number; title: string; mini: ReactNode; body: ReactNode }[] = [
  {
    n: 1,
    title: "Pick a model and sign in",
    mini: (
      <>
        <span className={row + " border-vm shadow-[inset_3px_0_0_var(--vm)]"}>
          Pi · isolated VM<em className="text-vm not-italic">Signed in</em>
        </span>
        <span className={row + " opacity-60"}>
          Codex<em className="text-ink-2 not-italic">Coming soon</em>
        </span>
      </>
    ),
    body: (
      <>
        The agent runs on <a href="https://github.com/earendil-works/pi">Pi</a>. Sign in with ChatGPT or Claude, or bring
        an API key. Choose a light model for everyday building and a stronger one the agent can ask for when a task is
        hard.
      </>
    ),
  },
  {
    n: 2,
    title: "Say what you want",
    mini: (
      <span className="flex items-center gap-2 rounded-[10px] border-[1.5px] border-line bg-surface px-[11px] py-2.5">
        <span className="rounded-md bg-vm px-[7px] py-0.5 text-[0.74rem] font-semibold text-surface">Build</span>
        /build an inbox for my work mail
      </span>
    ),
    body: (
      <>
        Plain messages are chat. Start with <kbd>/build</kbd> and the agent changes the app. It asks only for decisions and
        the steps only you can do, like signing in somewhere.
      </>
    ),
  },
  {
    n: 3,
    title: "Watch it appear",
    mini: (
      <span className="grid gap-[3px] rounded-[10px] border border-line bg-surface px-3 py-[11px]">
        <b className="font-semibold">an inbox for my work mail</b>
        <span className="text-ink-2">Added an Inbox sheet next to Send.</span>
        <span className="mt-1 flex gap-3 font-[560] text-vm">
          <span>Undo</span>
          <span>Show details</span>
        </span>
      </span>
    ),
    body: (
      <>
        New controls, rails, sheets, windows and themes show up while you watch. Nothing restarts. Undo on the build card
        puts things back.
      </>
    ),
  },
  {
    n: 4,
    title: "Keep going",
    mini: (
      <>
        <span className={row}>
          <b className="font-semibold">Morning summary</b>
          <span className="text-ink-2">Every day at 8:00</span>
        </span>
        <span className={row}>
          <b className="font-semibold">Inbox sync</b>
          <span className="text-ink-2">Every 15 minutes</span>
        </span>
      </>
    ),
    body: (
      <>
        Features become modules you can keep, change or remove. Jobs keep running while the window is closed, and the
        agent remembers what it built.
      </>
    ),
  },
];

export function Grows() {
  const data: TimelineEntry[] = steps.map((s) => ({
    title: (
      <>
        <span className="mr-[0.55em] font-[720] text-vm">{s.n}</span>
        {s.title}
      </>
    ),
    content: (
      <>
        <div className={mini} aria-hidden="true">
          {s.mini}
        </div>
        <p className="text-[0.97rem] text-ink-2">{s.body}</p>
      </>
    ),
  }));

  return (
    <section aria-labelledby="grows-title">
      <div className="wrap section">
        <Timeline data={data} header={<h2 id="grows-title">How an app grows</h2>} />
      </div>
    </section>
  );
}
