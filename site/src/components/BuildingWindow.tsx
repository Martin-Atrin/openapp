import "./BuildingWindow.css";

const requests = [
  { ask: "let me dictate to you", done: "Added a microphone next to Send." },
  { ask: "a sidebar with my chats", done: "Added a left sidebar with your chats." },
  { ask: "a cat that naps on my sidebar", done: "She's asleep at the bottom of the sidebar." },
  { ask: "make it more colourful", done: "New colour scheme applied." },
  { ask: "open my inbox in a sheet", done: "Inbox opens from the mail button." },
] as const;

/** The hero's self-building window: a 22 s CSS loop of five real requests. */
export function BuildingWindow() {
  return (
    <div className="bw" aria-hidden="true">
      <div className="win">
      <div className="win-bar"><i></i><i></i><i></i><span>OpenApp</span></div>
      <div className="win-body">
        <aside className="rail">
          <b>Chats</b>
          <span className="row on">Morning plan</span>
          <span className="row">Music studio</span>
          <span className="row">Proton mail</span>
          <span className="cat">
            <svg viewBox="0 0 64 40" width="64" height="40">
              <path d="M8 34c0-9 9-15 21-15s23 5 23 13c0 3-2 4-5 4H12c-3 0-4-1-4-2z" fill="currentColor" />
              <path d="M42 21l3-9 5 7M50 21l5-7 1 9" fill="currentColor" />
              <circle cx="48" cy="25" r="7" fill="currentColor" />
              <path d="M45 25.5c1 1 2 1 3 0M50 25.5c1 1 2 1 3 0" stroke="var(--surface)" strokeWidth="1.2" fill="none" strokeLinecap="round" />
              <path d="M9 33c-5 0-7-4-4-7" stroke="currentColor" strokeWidth="3" fill="none" strokeLinecap="round" />
            </svg>
            <span className="zz">z</span>
          </span>
        </aside>
        <div className="stage">
          <p className="hint">Tell the agent what to build.</p>

          <div className="cards">
            {requests.map((r, i) => (
              <div key={r.ask} className={`card c${i + 1}`}>
                <b>{r.ask}</b>
                <span className="busy">Building…</span>
                <span className="done">{r.done}</span>
              </div>
            ))}
          </div>

          <div className="sheet">
            <b>Inbox</b>
            <span className="mail"><i></i>Concert tickets for Friday</span>
            <span className="mail"><i></i>Your invoice from the studio</span>
            <span className="mail read"><i></i>Notes from Tuesday's call</span>
          </div>

          <div className="composer">
            <span className="typing">
              {requests.map((r, i) => (
                <span key={r.ask} className={`t t${i + 1}`}>{r.ask}</span>
              ))}
            </span>
            <span className="mic">
              <svg viewBox="0 0 24 24" width="15" height="15">
                <rect x="9" y="3" width="6" height="12" rx="3" fill="currentColor" />
                <path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
              </svg>
            </span>
            <span className="send">Send</span>
          </div>
        </div>
      </div>
      </div>
    </div>
  );
}
