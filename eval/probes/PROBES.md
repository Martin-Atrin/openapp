# OpenApp buildability probes (E1–E12)

Each probe is one build-mode request sent by the probe runner (`cargo run --example eval -- probes`, `src-tauri/examples/eval.rs`) to the in-app agent in a fresh scratch workspace.
The runner auto-answers question cards with the listed answer, auto-approves egress for hosts the probe needs,
cancels secret cards it cannot fill (then checks the agent continues correctly), and records the result.

**Required checks** are outcomes the prompt asks for and a user would notice. Implementation preferences (a channel
vs polling, xterm.js, WebGL fallback, Finder reveal, Now Playing …) are *optional*: reported in the loop report, never
scored. A check passes only on code that can work; where the user asks for something specific (E6: their own Telegram
account), a weaker substitute fails the check.

**Pass** = the stated end state is reached without the agent refusing ("can't", "not supported", "don't have"),
without a human editing code, and with only provider-forced human steps left open (listed per probe).
**Partial** = built and loads, but a step is missing or broken. **Fail** = refused, or nothing usable.

| id | prompt (sent with /build) | auto-answers / approvals | pass end state (checked by the runner) | human-only steps allowed |
|---|---|---|---|---|
| E1 | "Make me a Proton Mail client. Set up the Proton Mail Bridge in your VM, show me the login and anything it needs, then build an inbox view I can read mail in." | egress: proton.me, *.proton.me, github.com release hosts | the bridge installed as a declared `services` entry; a way for the user to log in (the bridge CLI in a terminal view, or secret cards); an inbox view that reads mail from the bridge over IMAP. *Optional:* service running / waiting for login, live channel, secret release | entering the Proton password / 2FA |
| E2 | "Build a small music studio like GarageBand: tracks, record from my mic, a metronome, a MIDI keyboard instrument, and export to my Music folder." | consent: host module (record + CoreMIDI + AVAudioEngine), mic; folder grant ~/Music | tracks in an editor view; mic recording that can actually work (`audio.record` host action, a host module, or `getUserMedia` in an html view; the kernel allows the mic in module iframes after consent); metronome; MIDI/keyboard instrument; export via files.save or folder grant. *Optional:* host module, AVAudioEngine, CoreMIDI, meters over a channel, play/stop shortcut | approving consent cards, macOS mic prompt |
| E3 | "Add a terminal and a code editor for my workspace, and a preview of any web server I start." | — | a terminal view bound to a PTY (`pty.open` used or a pty channel subscribed); editor view; oasvc preview of a running dev server. *Optional:* xterm.js | — |
| E4 | "I need LibreOffice inside OpenApp to edit documents." | egress: Debian mirrors (default) | `display` service running LibreOffice; a view showing the remote display; a document opens | — |
| E5 | "Build me a unified inbox for my Gmail and my work IMAP account." | Q: path → "IMAP with an app password"; secret cards cancelled | connector modules; inbox view; new mail arrives without reload (channel OR job OR polling in the view); credentials asked via secret cards (not chat). *Optional:* a channel, a job (each reported separately) | entering app passwords |
| E6 | "Connect my Telegram so I can read and reply to chats here." | egress: telegram hosts | **strict, the USER's chats** (a bot token does not satisfy it): a user-account client (TDLib / MTProto lib such as GramJS, Telethon) declared as a service; a login UI for phone + code and the API id/hash step; a chat list fed by the service; send through the service. *Optional:* chat list over a channel | entering phone code |
| E7 | "Show my Photos library as a grid and let me crop and export a picture." | consent: Photos host module | host module reading Photos; grid shows library thumbnails; crop view; export via files.save. *Optional:* thumbnails streamed over a channel | Photos permission prompt |
| E8 | "Give me a file manager for my Documents folder and a Markdown editor that opens .md files from Finder." | folder grant ~/Documents | folder grant used; browser view; editor; manifest `opens: [".md"]`. *Optional:* reveal in Finder (not asked) | folder grant panel |
| E9 | "Every morning at 8, summarise my inbox and today's calendar and send me a notification." | consent: EventKit host module; egress: Google / Microsoft calendar + OAuth hosts | job with a cron schedule; an agent (G10) with scoped tools; today's calendar from **either** an EventKit host module **or** a calendar API connector (Google Calendar / Microsoft Graph via net.fetch, signed in with OAuth or a secret card); notification with an action that opens the summary. *Optional:* which calendar path (reported) | Calendar permission prompt, or the calendar account sign-in |
| E10 | "Make a music player for the files in my Music folder with a mini player in the menu bar." | folder grant ~/Music | player view; tray mini player with play/pause. *Optional:* media keys / Now Playing | folder grant panel |
| E11 | "Build a small WebGPU game I can play full screen with a gamepad." | — | html view using WebGPU; fullscreen (window.open / requestFullscreen); gamepad input handled. *Optional:* WebGL fallback | — |
| E12 | "Connect my Home Assistant and show my lights; also find Bluetooth sensors near me." | egress: the HA host; consent: CoreBluetooth host module | HA connector via net.fetch + secret; lights view with toggles; host module scanning BLE, scan results shown in a view. *Optional:* results streamed over a channel | entering the HA token |

Loop results go to `LOOP-<n>.md` next to this file: per probe status, time, questions, cards, modules, services,
channels, shell commands, refusals, kernel gaps the agent named, and the next primitive to add.
