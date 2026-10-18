// Tiny safe markdown renderer: paragraphs, **bold**, *italic*, `code`, fenced code, lists, headings, links.
// Builds React elements only (never raw HTML), and only http(s)/mailto links.
import { Fragment, type ReactNode } from "react";
import { kernel } from "./kernel";

const INLINE = /(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(\[[^\]\n]+\]\([^)\s]+\))|(\*[^*\n]+\*)/g;

function safeUrl(u: string): string | null {
  return /^(https?:\/\/|mailto:)/i.test(u) ? u : null;
}

function openLink(url: string) {
  kernel.uiAction("chat", { host: "open_url", url }).then(
    (r) => {
      if (!r.ok) window.open(url, "_blank", "noopener,noreferrer");
    },
    () => window.open(url, "_blank", "noopener,noreferrer"),
  );
}

export function Inline({ text }: { text: string }): ReactNode {
  const out: ReactNode[] = [];
  let last = 0;
  let k = 0;
  for (const m of text.matchAll(INLINE)) {
    const i = m.index ?? 0;
    if (i > last) out.push(text.slice(last, i));
    const t = m[0];
    if (m[1]) out.push(<code key={k++} className="oa-md-code">{t.slice(1, -1)}</code>);
    else if (m[2]) out.push(<strong key={k++}>{t.slice(2, -2)}</strong>);
    else if (m[4]) out.push(<em key={k++}>{t.slice(1, -1)}</em>);
    else if (m[3]) {
      const mm = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(t);
      const url = mm ? safeUrl(mm[2]!) : null;
      if (mm && url) {
        out.push(
          <a key={k++} href={url} className="oa-md-link" rel="noopener noreferrer" target="_blank"
            onClick={(e) => { e.preventDefault(); openLink(url); }}>
            {mm[1]}
          </a>,
        );
      } else out.push(t);
    }
    last = i + t.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return <>{out.map((n, i) => <Fragment key={i}>{n}</Fragment>)}</>;
}

type Block =
  | { t: "p"; text: string }
  | { t: "h"; text: string }
  | { t: "code"; text: string; lang: string }
  | { t: "ul" | "ol"; items: string[] };

function parse(src: string): Block[] {
  const lines = src.replace(/\r\n/g, "\n").split("\n");
  const blocks: Block[] = [];
  let para: string[] = [];
  const flush = () => {
    if (para.length) blocks.push({ t: "p", text: para.join("\n") });
    para = [];
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    const fence = /^\s*```(\w*)\s*$/.exec(line);
    if (fence) {
      flush();
      const body: string[] = [];
      i++;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i]!)) body.push(lines[i++]!);
      blocks.push({ t: "code", text: body.join("\n"), lang: fence[1] ?? "" }); // unterminated fence streams as code
      continue;
    }
    const h = /^#{1,6}\s+(.*)$/.exec(line);
    if (h) { flush(); blocks.push({ t: "h", text: h[1]! }); continue; }
    const ul = /^\s*[-*+]\s+(.*)$/.exec(line);
    const ol = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    if (ul || ol) {
      flush();
      const t = ul ? "ul" : "ol";
      const prev = blocks[blocks.length - 1];
      if (prev && prev.t === t) prev.items.push((ul ?? ol)![1]!);
      else blocks.push({ t, items: [(ul ?? ol)![1]!] });
      continue;
    }
    if (!line.trim()) { flush(); continue; }
    para.push(line);
  }
  flush();
  return blocks;
}

export function Markdown({ text }: { text: string }) {
  const blocks = parse(text);
  return (
    <div className="oa-md">
      {blocks.map((b, i) => {
        switch (b.t) {
          case "p": return <p key={i}><Inline text={b.text} /></p>;
          case "h": return <p key={i} className="oa-md-h"><Inline text={b.text} /></p>;
          case "code": return <pre key={i} className="oa-md-pre" data-lang={b.lang || undefined}><code>{b.text}</code></pre>;
          case "ul": return <ul key={i}>{b.items.map((it, j) => <li key={j}><Inline text={it} /></li>)}</ul>;
          case "ol": return <ol key={i}>{b.items.map((it, j) => <li key={j}><Inline text={it} /></li>)}</ol>;
        }
      })}
    </div>
  );
}
