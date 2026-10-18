import { IconBrandGithub, IconDownload, IconListCheck, IconTopologyStar3 } from "@tabler/icons-react";
import { FloatingNav } from "@/components/ui/floating-navbar";
import { Spotlight } from "@/components/ui/spotlight-new";
import { FlipWords } from "@/components/ui/flip-words";
import { ContainerScroll } from "@/components/ui/container-scroll-animation";
import { BuildingWindow } from "@/components/BuildingWindow";

export const GITHUB = "https://github.com/Martin-Atrin/openapp";

export function Mark({ size = 22 }: { size?: number }) {
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} aria-hidden="true">
      <rect x="2.5" y="2.5" width="19" height="19" rx="5" fill="none" stroke="currentColor" strokeWidth="2" />
      <path d="M8 15.5h8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}

const navLinks = [
  { name: "How it works", link: "#how", icon: <IconTopologyStar3 size={18} stroke={1.8} /> },
  { name: "What it built", link: "#proof", icon: <IconListCheck size={18} stroke={1.8} /> },
  { name: "Get it", link: "#get", icon: <IconDownload size={18} stroke={1.8} /> },
];

export function TopBar() {
  return (
    <>
      <header className="wrap relative z-20 flex items-center justify-between gap-4 py-5">
        <a className="wordmark" href="./" aria-label="OpenApp home">
          <Mark />
          OpenApp
        </a>
        <nav aria-label="Site" className="flex flex-wrap gap-[clamp(12px,2.4vw,28px)]">
          {navLinks.map((l) => (
            <a key={l.link} href={l.link} className="hidden text-[0.95rem] text-ink-2 no-underline hover:text-ink sm:inline">
              {l.name}
            </a>
          ))}
          <a href={GITHUB} className="text-[0.95rem] font-[560] text-ink no-underline">
            GitHub
          </a>
        </nav>
      </header>
      <FloatingNav
        navItems={navLinks}
        cta={{ name: "GitHub", link: GITHUB, icon: <IconBrandGithub size={17} stroke={1.8} /> }}
      />
    </>
  );
}

export function Hero() {
  return (
    <section className="relative" aria-labelledby="hero-title">
      <div
        className="pointer-events-none absolute inset-x-0 -top-24 h-[110svh] overflow-hidden [mask-image:linear-gradient(to_bottom,black_55%,transparent)]"
        aria-hidden="true"
      >
        <Spotlight leftOnly translateY={-300} width={620} height={1300} smallWidth={260} xOffset={70} duration={8} />
      </div>

      <div className="wrap relative z-10 pt-[clamp(24px,5vw,64px)]">
        <div className="max-w-[44rem]">
          <h1
            id="hero-title"
            className="mb-[0.42em] text-[clamp(2.7rem,1.3rem+5.6vw,5.4rem)] leading-[0.96] font-[740] tracking-[-0.04em]"
          >
            Your app,
            <br />
            built by asking.
          </h1>
          <p className="text-[clamp(1.08rem,1rem+0.4vw,1.25rem)] text-ink-2">
            One interface on your Mac that its own agent builds around your life. Start from a clean slate and ask for
            what you need —{" "}
            <span className="sr-only">a feature, a theme, a pet, a window</span>
            <span aria-hidden="true">
              <FlipWords
                words={["a feature", "a theme", "a pet", "a window"]}
                staticText="a feature, a theme, a pet, a window"
                duration={2600}
                className="font-[560] text-vm"
              />
            </span>
            . It appears while you watch.
          </p>
          <div className="mt-[1.6em] mb-[1.1em] flex flex-wrap gap-3">
            <a className="btn btn-primary" href="#get">
              Download for macOS
            </a>
            <a className="btn" href={GITHUB}>
              Read the source
            </a>
          </div>
          <p className="fine">Early alpha for Apple silicon Macs with macOS 26. Open source.</p>
        </div>
      </div>

      <figure
        className="wrap relative z-10 m-0 pt-[clamp(28px,4vw,56px)] pb-[clamp(48px,7vw,96px)]"
        aria-label="Animation: an empty OpenApp window grows a microphone button, a chat sidebar, a napping cat, a new colour scheme and an inbox sheet as five requests arrive."
      >
        <ContainerScroll>
          <BuildingWindow />
        </ContainerScroll>
        <figcaption className="mx-auto mt-4 max-w-[960px] text-[0.9rem] text-ink-2">
          <span className="block max-w-[56ch]">
            Five requests, no restarts. Each change is a module the agent wrote, and each one can be undone.
          </span>
        </figcaption>
      </figure>
    </section>
  );
}
