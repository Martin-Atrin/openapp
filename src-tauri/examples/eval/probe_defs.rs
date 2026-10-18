//! Probe table (E1–E12), the facts collected after a probe, and the pass predicates.
//! Mirrors eval/probes/PROBES.md. Required checks = outcomes the prompt asks for that a user would notice;
//! implementation preferences are `opt` (reported, not scored). Never pass a check on code that cannot work.
//! Everything is read from JSON (`describe`, `snapshot`, manifests on disk,
//! the research log) so the runner keeps compiling while library types change.
#![allow(dead_code)]

use serde_json::Value;
use std::collections::BTreeSet;

pub struct Folder {
    /// grant name (mounted at /mnt/<name>)
    pub name: &'static str,
    /// folder under the user's home that the probe names, e.g. "Music"
    pub home_dir: &'static str,
    pub purpose: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// counts toward pass
    Req,
    /// reported, does not change the status
    Opt,
    /// cannot be verified headless (needs a human, a GUI or a live account)
    Manual,
}

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub kind: Kind,
}

fn req(name: &str, ok: bool) -> Check {
    Check { name: name.into(), ok, kind: Kind::Req }
}
fn opt(name: &str, ok: bool) -> Check {
    Check { name: name.into(), ok, kind: Kind::Opt }
}
fn manual(name: &str) -> Check {
    Check { name: name.into(), ok: false, kind: Kind::Manual }
}

pub struct Probe {
    pub id: &'static str,
    pub title: &'static str,
    pub prompt: &'static str,
    /// substrings matched against question option labels (case-insensitive)
    pub answers: &'static [&'static str],
    /// egress allow patterns: "host", "*.host" (host + subdomains), "ip:149.154." (prefix)
    pub egress: &'static [&'static str],
    /// consent cards to approve: host_module | microphone | camera | lan | clipboard | shortcut
    pub consent: &'static [&'static str],
    pub folders: &'static [Folder],
    pub end_state: &'static str,
    pub human: &'static str,
    pub checks: fn(&Facts) -> Vec<Check>,
}

const MUSIC: Folder = Folder { name: "music", home_dir: "Music", purpose: "Your music files" };
const DOCS: Folder = Folder { name: "documents", home_dir: "Documents", purpose: "Your documents" };

pub const PROBES: &[Probe] = &[
    Probe {
        id: "E1",
        title: "Proton Mail via Bridge",
        prompt: "Make me a Proton Mail client. Set up the Proton Mail Bridge in your VM, show me the login and anything it needs, then build an inbox view I can read mail in.",
        answers: &[],
        egress: &["proton.me", "*.proton.me", "github.com", "api.github.com", "*.githubusercontent.com"],
        consent: &[],
        folders: &[],
        end_state: "bridge installed as a declared service; a way to log in (bridge CLI in a terminal view, or secret cards); an inbox view that reads mail from the bridge. Optional: service running, live channel, secret release",
        human: "entering the Proton password / 2FA",
        checks: checks_e1,
    },
    Probe {
        id: "E2",
        title: "Music studio",
        prompt: "Build a small music studio like GarageBand: tracks, record from my mic, a metronome, a MIDI keyboard instrument, and export to my Music folder.",
        answers: &[],
        egress: &[],
        consent: &["host_module", "microphone"],
        folders: &[MUSIC],
        end_state: "tracks in an editor view; mic recording that can work (audio.record, a host module, or getUserMedia in an html view); metronome; MIDI/keyboard instrument; export via files.save or folder grant. Optional: native engine, meters, shortcut",
        human: "approving consent cards, macOS mic prompt",
        checks: checks_e2,
    },
    Probe {
        id: "E3",
        title: "Terminal, editor, preview",
        prompt: "Add a terminal and a code editor for my workspace, and a preview of any web server I start.",
        answers: &[],
        egress: &[],
        consent: &[],
        folders: &[],
        end_state: "terminal view bound to a PTY (pty.open or a pty channel); editor view; oasvc preview of a running dev server. Optional: xterm.js",
        human: "none",
        checks: checks_e3,
    },
    Probe {
        id: "E4",
        title: "LibreOffice in a display",
        prompt: "I need LibreOffice inside OpenApp to edit documents.",
        answers: &[],
        egress: &["deb.debian.org", "security.debian.org", "*.debian.org"],
        consent: &[],
        folders: &[],
        end_state: "display service running LibreOffice; a view showing the remote display; a document opens",
        human: "none",
        checks: checks_e4,
    },
    Probe {
        id: "E5",
        title: "Unified inbox (Gmail + IMAP)",
        prompt: "Build me a unified inbox for my Gmail and my work IMAP account.",
        answers: &["IMAP with an app password", "app password", "imap"],
        egress: &[],
        consent: &[],
        folders: &[],
        end_state: "connector modules; inbox view; new mail arrives without reload (channel OR job OR polling); credentials via secret cards (not chat)",
        human: "entering app passwords",
        checks: checks_e5,
    },
    Probe {
        id: "E6",
        title: "Telegram client",
        prompt: "Connect my Telegram so I can read and reply to chats here.",
        answers: &[],
        egress: &["telegram.org", "*.telegram.org", "t.me", "*.t.me", "ip:149.154.", "ip:91.108.", "ip:95.161."],
        consent: &[],
        folders: &[],
        end_state: "a user-account Telegram client (TDLib / MTProto lib; the bot API does not count) running as a service; phone + code login UI and API id/hash step; chat list from the service; send works",
        human: "entering the phone code",
        checks: checks_e6,
    },
    Probe {
        id: "E7",
        title: "Photos grid and crop",
        prompt: "Show my Photos library as a grid and let me crop and export a picture.",
        answers: &[],
        egress: &[],
        consent: &["host_module"],
        folders: &[],
        end_state: "host module reading Photos; grid shows library thumbnails; crop view; export via files.save. Optional: thumbnails over a channel",
        human: "Photos permission prompt",
        checks: checks_e7,
    },
    Probe {
        id: "E8",
        title: "File manager and Markdown editor",
        prompt: "Give me a file manager for my Documents folder and a Markdown editor that opens .md files from Finder.",
        answers: &[],
        egress: &[],
        consent: &[],
        folders: &[DOCS],
        end_state: "folder grant used; browser view; editor; manifest opens: [\".md\"]. Optional: reveal in Finder",
        human: "folder grant panel",
        checks: checks_e8,
    },
    Probe {
        id: "E9",
        title: "Morning summary job",
        prompt: "Every morning at 8, summarise my inbox and today's calendar and send me a notification.",
        answers: &[],
        // a calendar API connector is as good as EventKit: allow the Google / Microsoft calendar + OAuth hosts
        egress: &["www.googleapis.com", "oauth2.googleapis.com", "accounts.google.com", "graph.microsoft.com", "login.microsoftonline.com"],
        consent: &["host_module"],
        folders: &[],
        end_state: "job with a cron schedule; an agent (G10) with scoped tools; today's calendar via an EventKit host module or a calendar API connector (Google / Microsoft via net.fetch / OAuth); notification with an action that opens the summary",
        human: "Calendar permission prompt or the calendar account sign-in",
        checks: checks_e9,
    },
    Probe {
        id: "E10",
        title: "Music player with menu-bar mini player",
        prompt: "Make a music player for the files in my Music folder with a mini player in the menu bar.",
        answers: &[],
        egress: &[],
        consent: &["host_module"],
        folders: &[MUSIC],
        end_state: "player view; tray mini player with play/pause. Optional: media keys / Now Playing",
        human: "folder grant panel",
        checks: checks_e10,
    },
    Probe {
        id: "E11",
        title: "WebGPU gamepad game",
        prompt: "Build a small WebGPU game I can play full screen with a gamepad.",
        answers: &[],
        egress: &[],
        consent: &[],
        folders: &[],
        end_state: "html view using WebGPU; fullscreen; gamepad input handled. Optional: WebGL fallback",
        human: "none",
        checks: checks_e11,
    },
    Probe {
        id: "E12",
        title: "Home Assistant lights and BLE",
        prompt: "Connect my Home Assistant and show my lights; also find Bluetooth sensors near me.",
        answers: &[],
        egress: &["homeassistant", "homeassistant.local", "*.local", "*.ui.nabu.casa", "ip:192.168.", "ip:10."],
        consent: &["host_module"],
        folders: &[],
        end_state: "HA connector via net.fetch + secret; lights view with toggles; host module scanning BLE with the results shown in a view. Optional: results streamed",
        human: "entering the HA token",
        checks: checks_e12,
    },
];

// ---------- facts ----------

#[derive(Default)]
pub struct Facts {
    pub desc: Value,
    pub snap: Value,
    pub chat: Vec<Value>,
    pub research: Vec<Value>,
    /// (module id, parsed openapp.json)
    pub manifests: Vec<(String, Value)>,
    /// (path relative to the workspace, lowercase text) for module sources
    pub files: Vec<(String, String)>,
    /// all module source text, lowercase
    pub blob: String,
    /// agent messages, lowercase
    pub agent_text: String,
    /// tool chip titles + commands, lowercase
    pub tools_text: String,
    pub host_actions: BTreeSet<String>,
    pub secret_cards: Vec<Value>,
    pub question_cards: Vec<Value>,
    pub granted_folders: Vec<String>,
}

impl Facts {
    /// any of the words in module sources
    pub fn src(&self, words: &[&str]) -> bool {
        words.iter().any(|w| self.blob.contains(&w.to_lowercase()))
    }
    pub fn src_all(&self, words: &[&str]) -> bool {
        words.iter().all(|w| self.blob.contains(&w.to_lowercase()))
    }
    /// any of the words in view files (modules/<id>/views/*)
    pub fn view_src(&self, words: &[&str]) -> bool {
        self.files.iter().any(|(p, t)| p.contains("/views/") && words.iter().any(|w| t.contains(&w.to_lowercase())))
    }
    /// any of the words in code that is not a view (servers, services, host modules, package files), docs excluded
    pub fn backend_src(&self, words: &[&str]) -> bool {
        self.files.iter().any(|(p, t)| !p.contains("/views/") && !p.ends_with(".md") && words.iter().any(|w| t.contains(&w.to_lowercase())))
    }
    /// any of the words in a host module's Swift sources (modules/<id>/host/*)
    pub fn host_src(&self, words: &[&str]) -> bool {
        self.files.iter().any(|(p, t)| p.contains("/host/") && words.iter().any(|w| t.contains(&w.to_lowercase())))
    }
    /// a view polls a module: setInterval together with a call into the module or host
    pub fn view_polls(&self) -> bool {
        self.files.iter().any(|(p, t)| p.contains("/views/") && t.contains("setinterval") && (t.contains("openapp.call") || t.contains("openapp.host") || t.contains("fetch(")))
    }
    /// used by module sources, agent host actions, or agent tool calls
    pub fn used(&self, needle: &str) -> bool {
        let n = needle.to_lowercase();
        self.blob.contains(&n) || self.tools_text.contains(&n) || self.host_actions.iter().any(|a| a.to_lowercase().contains(&n))
    }
    pub fn manifest_key(&self, key: &str) -> bool {
        self.manifests.iter().any(|(_, m)| m.get(key).is_some_and(|v| !v.is_null() && v != &Value::Array(vec![]) && v != &Value::Object(Default::default())))
    }
    pub fn manifest_vals(&self, key: &str) -> Vec<&Value> {
        self.manifests.iter().filter_map(|(_, m)| m.get(key)).filter(|v| !v.is_null()).collect()
    }
    pub fn manifest_text(&self) -> String {
        self.manifests.iter().map(|(_, m)| m.to_string().to_lowercase()).collect::<Vec<_>>().join("\n")
    }
    /// describe.services.running
    pub fn services(&self) -> Vec<Value> {
        self.desc.pointer("/services/running").and_then(Value::as_array).cloned().unwrap_or_default()
    }
    pub fn service_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.services().iter().filter_map(|s| s["id"].as_str().map(String::from)).collect();
        // declared but not (yet) in describe
        for (id, m) in &self.manifests {
            if let Some(a) = m.get("services").and_then(Value::as_array) {
                for s in a {
                    if let Some(n) = s["name"].as_str() {
                        let full = format!("{id}/{n}");
                        if !v.contains(&full) {
                            v.push(full);
                        }
                    }
                }
            }
        }
        v
    }
    /// services whose id/command/manifest mention a word, with their live state ("" if only declared)
    pub fn services_matching(&self, words: &[&str]) -> Vec<(String, String)> {
        let live = self.services();
        let mut out = Vec::new();
        for s in &live {
            let hay = format!("{} {}", s["id"], s["command"]).to_lowercase();
            if words.iter().any(|w| hay.contains(*w)) {
                out.push((s["id"].as_str().unwrap_or("").to_string(), s["state"].as_str().unwrap_or("").to_string()));
            }
        }
        for (id, m) in &self.manifests {
            if let Some(a) = m.get("services").and_then(Value::as_array) {
                for s in a {
                    let hay = s.to_string().to_lowercase();
                    let full = format!("{id}/{}", s["name"].as_str().unwrap_or(""));
                    if words.iter().any(|w| hay.contains(*w)) && !out.iter().any(|(n, _)| n == &full) {
                        out.push((full, String::new()));
                    }
                }
            }
        }
        out
    }
    pub fn topics(&self) -> Vec<String> {
        self.desc.pointer("/channels/topics").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| t["topic"].as_str().map(String::from)).collect()).unwrap_or_default()
    }
    /// a view/channel binding: module sources subscribe or bind $stream, or the manifest declares channels
    pub fn live_channel(&self) -> bool {
        self.src(&["subscribe(", "$stream"]) || self.manifest_key("channels") || self.topics().iter().any(|t| !t.starts_with("kernel/"))
    }
    pub fn host_modules(&self) -> Vec<String> {
        self.manifests.iter().filter(|(_, m)| m.get("host").is_some_and(|h| !h.is_null())).map(|(id, _)| id.clone()).collect()
    }
    pub fn has_view(&self, kind: &str) -> bool {
        self.manifests.iter().any(|(_, m)| m.get("views").and_then(Value::as_object).is_some_and(|v| v.values().any(|x| x["kind"] == kind)))
    }
    pub fn views_count(&self) -> usize {
        self.manifests.iter().map(|(_, m)| m.get("views").and_then(Value::as_object).map(|v| v.len()).unwrap_or(0)).sum()
    }
    pub fn modules_loaded(&self) -> Vec<(String, String)> {
        self.snap["modules"].as_array().map(|a| a.iter().map(|m| (m["id"].as_str().unwrap_or("").to_string(), m["status"].as_str().unwrap_or("").to_string())).collect()).unwrap_or_default()
    }
    pub fn presented_or_placed(&self) -> bool {
        self.snap["presented"].as_array().is_some_and(|a| !a.is_empty()) || self.snap["layout"]["placements"].as_array().is_some_and(|a| !a.is_empty())
    }
    pub fn secrets_requested(&self) -> usize {
        self.secret_cards.len()
    }
    pub fn jobs_declared(&self) -> bool {
        self.manifest_key("jobs") || self.used("job.create")
    }
}

// ---------- predicates ----------

fn checks_e1(f: &Facts) -> Vec<Check> {
    let bridge = f.services_matching(&["bridge", "protonmail", "proton-bridge"]);
    let live = bridge.iter().any(|(_, s)| matches!(s.as_str(), "running" | "starting"));
    // a way to log in: the bridge CLI in a terminal view, or secret cards for the credentials
    let cli_in_terminal = f.view_src(&["pty.open"]) && f.src(&["--cli", "bridge --cli"]);
    vec![
        req("bridge installed as a declared service (services entry)", !bridge.is_empty()),
        req("a way for the user to log in (bridge CLI in a terminal view, or secret cards)", cli_in_terminal || f.secrets_requested() > 0),
        req("inbox view that reads mail from the bridge (IMAP)", f.views_count() > 0 && f.view_src(&["inbox", "mail"]) && f.backend_src(&["imap", "1143"])),
        opt("bridge service running / waiting for login", live),
        opt("inbox view bound to a live channel", f.live_channel()),
        opt("secret release requested for bridge credentials", f.used("secret.release")),
        manual("Proton password / 2FA entered by the human"),
    ]
}

fn checks_e2(f: &Facts) -> Vec<Check> {
    // recording that can actually work: the kernel action, a host module, or getUserMedia in an html view
    // (the kernel allows the microphone in module iframes after the user's consent)
    let mic = f.view_src(&["audio.record", "getusermedia"])
        || !f.host_modules().is_empty() && f.host_src(&["avaudioengine", "avaudiorecorder", "avaudiosession", "avcapture"]);
    vec![
        req("tracks in an editor view", f.views_count() > 0 && f.src(&["track"])),
        req("mic recording that can work (audio.record, native audio, or getUserMedia in an html view)", mic),
        req("metronome", f.src(&["metronome", "click track", "bpm"])),
        req("MIDI / keyboard instrument", f.src(&["webmidi", "requestmidiaccess", "coremidi", "midiclient", "keydown", "keyup"]) && f.src(&["oscillator", "audiocontext", "avaudioengine", "avaudiounitsampler", "synth"])),
        req("export via files.save or folder grant", f.used("files.save") || f.src(&["/mnt/music"]) || !f.granted_folders.is_empty() && f.used("files.")),
        opt("host module declared", !f.host_modules().is_empty()),
        opt("native engine (AVAudioEngine)", f.src(&["avaudioengine"])),
        opt("CoreMIDI used", f.src(&["coremidi", "midiclient"])),
        opt("live meters over a channel", f.src(&["meter"]) && f.live_channel()),
        opt("keyboard shortcut for play/stop", f.used("shortcut.register") || f.src(&["accelerator"])),
        manual("mic consent + macOS mic prompt"),
    ]
}

fn checks_e3(f: &Facts) -> Vec<Check> {
    vec![
        req("terminal view bound to a PTY (pty.open used or a pty channel subscribed)", f.view_src(&["pty.open"]) || f.view_src(&["/pty/"]) && f.view_src(&["subscribe("])),
        req("editor view (Monaco / CodeMirror / textarea)", f.src(&["monaco", "codemirror", "<textarea", "contenteditable"])),
        req("web server preview (oasvc:// or a service port with expose view)", f.src(&["oasvc://"]) || f.manifest_text().contains("\"expose\":\"view\"")),
        opt("xterm.js in a view", f.src(&["xterm"])),
        manual("PTY echoes typed input (needs a live run of the view)"),
    ]
}

fn checks_e4(f: &Facts) -> Vec<Check> {
    let display = f.manifest_key("display");
    let lo = f.manifest_text().contains("libreoffice") || f.tools_text.contains("libreoffice") || f.tools_text.contains("soffice");
    vec![
        req("display declared in a manifest", display),
        req("LibreOffice installed / started (apt or command)", lo || f.src(&["soffice", "libreoffice"])),
        req("view showing the remote display (display.url / oasvc / vnc / xpra)", f.src(&["display.url", "oasvc://", "novnc", "xpra", "vnc"])),
        manual("a document opens in the display (needs a live view)"),
    ]
}

fn checks_e5(f: &Facts) -> Vec<Check> {
    let mods = f.manifests.len();
    let asked_in_chat = f.question_cards.iter().any(|q| {
        q["questions"].as_array().is_some_and(|a| a.iter().any(|x| x["isSecret"] != true && x["question"].as_str().is_some_and(|t| {
            let t = t.to_lowercase();
            t.contains("paste your password") || t.contains("enter your password") || t.contains("app password?") && x["options"].as_array().is_none_or(|o| o.is_empty())
        })))
    });
    vec![
        req("connector modules (>= 1 mail/imap module)", mods > 0 && f.src(&["imap", "gmail"])),
        req("inbox view", f.views_count() > 0 && f.src(&["inbox"])),
        // one of the three is enough: the user must see new mail without reloading the view
        req("new mail arrives without reload (channel OR job OR polling in the view)", f.live_channel() || f.jobs_declared() || f.view_polls()),
        req("credentials requested via secret cards", f.secrets_requested() > 0),
        req("no credentials asked in chat", !asked_in_chat),
        opt("channel for new mail", f.live_channel()),
        opt("job for periodic sync", f.jobs_declared()),
        manual("app passwords entered by the human"),
    ]
}

fn checks_e6(f: &Facts) -> Vec<Check> {
    // The USER's chats: a user-account client (TDLib / an MTProto library). The bot API does not satisfy this.
    const LIBS: &[&str] = &["tdlib", "td_json", "tdjson", "telethon", "gramjs", "pyrogram", "mtproto", "mtcute", "tdl-", "telegram/tl", "tglib"];
    let tg = f.services_matching(&["tdlib", "tdl", "telegram", "telethon", "gramjs", "pyrogram", "tdjson", "mtproto", "mtcute"]);
    let lib_in_backend = f.backend_src(LIBS);
    let creds_step = f.src(&["api_id", "api_hash", "api id", "api hash", "my.telegram.org"]) || f.secret_cards.iter().any(|c| {
        let t = c.to_string().to_lowercase();
        t.contains("api_id") || t.contains("api_hash") || t.contains("api id") || t.contains("api hash") || t.contains("my.telegram.org")
    });
    vec![
        req("user-account Telegram client (TDLib / MTProto lib) running as a service", !tg.is_empty() && lib_in_backend),
        req("login UI for phone + code", f.view_src(&["phone"]) && f.view_src(&["code"])),
        req("API id/hash step (secret card or input)", creds_step),
        req("chat list view fed by the service", f.views_count() > 0 && f.view_src(&["chat"]) && f.view_src(&["openapp.call", "openapp.host", "subscribe(", "$stream", "fetch("])),
        req("send path against the service", lib_in_backend && f.src(&["sendmessage", "send_message", "send("])),
        opt("chat list bound to a channel", f.live_channel()),
        manual("phone code entered by the human; real send"),
    ]
}

fn checks_e7(f: &Facts) -> Vec<Check> {
    vec![
        req("host module declared", !f.host_modules().is_empty()),
        req("host module reads Photos (PhotoKit)", f.host_src(&["phphotolibrary", "phasset", "photokit", "import photos"])),
        req("grid shows library thumbnails", f.view_src(&["grid"]) && f.view_src(&["<img", "data:image", "canvas", "background-image"]) && f.src(&["thumb", "b64", "base64", "requestimage"])),
        req("crop view", f.src(&["crop"]) && f.views_count() > 0),
        req("export via files.save", f.used("files.save")),
        opt("thumbnails streamed (channel)", f.live_channel() && f.src(&["thumb"])),
        manual("Photos permission prompt"),
    ]
}

fn checks_e8(f: &Facts) -> Vec<Check> {
    let opens: Vec<String> = f.manifest_vals("opens").iter().flat_map(|v| v.as_array().cloned().unwrap_or_default()).filter_map(|x| x.as_str().map(String::from)).collect();
    vec![
        req("folder grant used (/mnt/documents or files.grant_folder)", f.src(&["/mnt/documents", "/mnt/"]) || f.used("files.grant_folder") || f.used("files.")),
        req("file browser view", f.views_count() > 0 && f.src(&["readdir", "list", "browse", "folder", "files"])),
        req("Markdown editor", f.src(&["markdown", ".md"]) && f.src(&["<textarea", "codemirror", "monaco", "contenteditable"])),
        req("manifest opens: [\".md\"]", opens.iter().any(|o| o.contains("md"))),
        opt("reveal in Finder (files.reveal)", f.used("files.reveal")),
        manual("folder grant panel"),
    ]
}

fn checks_e9(f: &Facts) -> Vec<Check> {
    let jobs = f.manifest_vals("jobs");
    let cron = jobs.iter().any(|v| v.to_string().contains("* *") || v.to_string().contains("\"cron\"") || v.to_string().contains("8"));
    vec![
        req("job with a cron schedule at 8", !jobs.is_empty() && cron || f.used("job.create") && f.src(&["0 8 * * *", "8:00"])),
        req("agent module (manifest agents) with scoped tools", f.manifest_key("agents") && f.manifest_text().contains("tools")),
        req("today's calendar: EventKit host module or a calendar API connector (Google / Microsoft via net.fetch / OAuth)", calendar_source(f)),
        opt("EventKit host module", eventkit(f)),
        opt("calendar API connector", calendar_api(f)),
        req("notification with an action that opens the summary", f.used("notify") && f.src(&["actions", "action"])),
        manual("Calendar permission prompt or calendar account sign-in; a real 8:00 run"),
    ]
}

fn eventkit(f: &Facts) -> bool {
    !f.host_modules().is_empty() && f.src(&["ekeventstore", "eventkit"])
}

/// A calendar API connector: Google Calendar or Microsoft Graph reached through net.fetch, signed in via OAuth
/// (oauth.authorize / oauth.device) or a token from a secret card.
fn calendar_api(f: &Facts) -> bool {
    let api = f.backend_src(&["googleapis.com/calendar", "calendar/v3", "graph.microsoft.com"]) && f.src(&["calendar", "calendarview", "/events"]);
    let auth = f.used("oauth.authorize") || f.used("oauth.device") || f.secrets_requested() > 0;
    api && f.used("net.fetch") && auth
}

fn calendar_source(f: &Facts) -> bool {
    eventkit(f) || calendar_api(f)
}

fn checks_e10(f: &Facts) -> Vec<Check> {
    vec![
        req("player view", f.views_count() > 0 && f.src(&["<audio", "audio("]) ),
        req("music folder granted / used (/mnt/music)", f.src(&["/mnt/music"]) || f.used("files.")),
        req("tray mini player with play/pause (tray.set)", f.used("tray.set") && f.src(&["pause"])),
        opt("media keys / Now Playing via host module", !f.host_modules().is_empty() && f.src(&["mpnowplayinginfocenter", "mpremotecommandcenter", "mediaremote", "nowplaying", "mediasession"])),
        manual("folder grant panel"),
    ]
}

fn checks_e11(f: &Facts) -> Vec<Check> {
    vec![
        req("html view", f.has_view("html")),
        req("WebGPU (navigator.gpu)", f.src(&["navigator.gpu"])),
        req("fullscreen via window.open / requestFullscreen", f.used("window.open") || f.src(&["requestfullscreen", "fullscreen"])),
        req("gamepad input handled", f.src(&["getgamepads", "gamepadconnected"])),
        opt("WebGL fallback", f.src(&["webgl"])),
        manual("actually plays with a gamepad (needs a window)"),
    ]
}

fn checks_e12(f: &Facts) -> Vec<Check> {
    vec![
        req("HA connector via net.fetch", f.used("net.fetch")),
        req("HA token requested via a secret card", f.secrets_requested() > 0),
        req("lights view with toggles", f.views_count() > 0 && f.src(&["light.", "\"toggle\"", "toggle"])),
        req("host module scanning BLE (CoreBluetooth)", !f.host_modules().is_empty() && f.host_src(&["cbcentralmanager", "corebluetooth"])),
        req("BLE scan results shown in a view", f.view_src(&["bluetooth", "ble", "sensor"]) && f.view_src(&["rssi", "peripheral", "devices", "name"]) && f.src(&["scan"])),
        opt("BLE results streamed (channel.publish)", f.src(&["channel.publish"]) || f.live_channel()),
        manual("HA token entered by the human; Bluetooth prompt"),
    ]
}

// ---------- egress
// ---------- egress pattern matching ----------

/// "host" exact, "*.host" host and subdomains, "ip:1.2." prefix.
pub fn egress_matches(pattern: &str, host: &str) -> bool {
    let host = host.split(':').next().unwrap_or(host).to_lowercase();
    if let Some(p) = pattern.strip_prefix("ip:") {
        return host.starts_with(p);
    }
    match pattern.strip_prefix("*.") {
        Some(base) => host == base || host.ends_with(&format!(".{base}")),
        None => host == pattern,
    }
}
