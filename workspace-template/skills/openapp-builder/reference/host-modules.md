# Host modules (the rest of macOS)

Anything macOS can do can be built: Contacts, Calendar/Reminders (EventKit), Photos, location, Shortcuts, AppleScript,
Spotlight, NSWorkspace… A **host module** is Swift that the kernel compiles and runs **on the Mac** (not in the VM),
after the user allows it on a consent card.

Rules: host services first; a host module only for the rest; one capability per method; the smallest code possible;
a one-sentence `summary` (shown on the card). Any source change asks the user again. macOS may then ask for its own
permission (e.g. Calendars).

```json
// modules/today/openapp.json
{ "id": "today", "name": "Today", "host": { "swift": "host/main.swift", "summary": "Reads today's events from your calendars." },
  "views": { "list": { "kind": "spec", "file": "views/list.json" } } }
```
```swift
// modules/today/host/main.swift — JSON-RPC 2.0, one JSON object per line on stdin/stdout
import Foundation
import EventKit

let store = EKEventStore()
func reply(_ id: Any, _ result: Any?, _ error: String?) {
    var msg: [String: Any] = ["jsonrpc": "2.0", "id": id]
    if let error { msg["error"] = ["code": -32000, "message": error] } else { msg["result"] = result ?? NSNull() }
    var data = try! JSONSerialization.data(withJSONObject: msg); data.append(0x0A)
    FileHandle.standardOutput.write(data)
}
func todayEvents() throws -> Any {
    let sem = DispatchSemaphore(value: 0); var granted = false
    store.requestFullAccessToEvents { ok, _ in granted = ok; sem.signal() }
    sem.wait()
    guard granted else { throw NSError(domain: "today", code: 1, userInfo: [NSLocalizedDescriptionKey: "Calendar access was not allowed"]) }
    let start = Calendar.current.startOfDay(for: Date()), end = start.addingTimeInterval(86400)
    let pred = store.predicateForEvents(withStart: start, end: end, calendars: nil)
    return store.events(matching: pred).map { ["title": $0.title ?? "", "start": ISO8601DateFormatter().string(from: $0.startDate)] }
}
while let line = readLine() {
    guard let o = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any], let m = o["method"] as? String else { continue }
    let id = o["id"] ?? NSNull()
    do {
        switch m {
        case "methods": reply(id, [["name": "today_events", "description": "Today's calendar events"]], nil)
        case "today_events": reply(id, try todayEvents(), nil)
        default: reply(id, nil, "unknown method \(m)")
        }
    } catch { reply(id, nil, error.localizedDescription) }
}
```
Load it with `openapp_module_load` (the call waits while the user reads the card; compiler errors come back as
`diagnostics [{file, line, message}]`). Call it with `host_call {module:"today", method:"today_events"}`, from a server
with `POST $OPENAPP_HOST_URL/today/today_events`, or from a view with
`{"host":"host.call","module":"today","method":"today_events"}`.

Debugging: the host part's stderr (`FileHandle.standardError`, `print` to stderr, crashes) goes to
`.openapp/logs/<module>.host.log` in the workspace (rotated to `.host.log.1` at 1 MB; also `host_part.log` in the
load result). Read it when a call fails. A call that waits 60 s fails with "the host module did not answer in 60 s —
macOS may be showing a permission prompt to the user" when the module imports a privacy framework (EventKit, Contacts,
Photos, AVFoundation, CoreLocation, CoreBluetooth…): tell the user to answer the macOS dialog, then call again.

Host modules can publish on channels: JSON-RPC notifications `channel.publish {topic, data}` and
`channel.subscribe {topic}` on stdout; messages come back as `channel.message` (see `channels.md`).

## Long-running host modules with windows (audio, plugins, devices)
`"host": { "swift": "host/main.swift", "summary": "…", "service": true, "windows": true }` keeps the host part running
(restarted if it crashes) and tells the user on the card that it opens windows. Use the kernel runtime `OpenAppHost`
(no JSON-RPC boilerplate; handlers run on the main thread, AppKit works):
```swift
import AppKit
import AVFoundation
let engine = AVAudioEngine()
engine.mainMixerNode.installTap(onBus: 0, bufferSize: 1024, format: nil) { buf, _ in
    let ch = buf.floatChannelData![0]; var peak: Float = 0
    for i in 0..<Int(buf.frameLength) { peak = max(peak, abs(ch[i])) }
    OpenAppHost.publish("host:daw/meters", ["l": peak])          // ≤ 60 Hz is plenty
}
OpenAppHost.method("play", "Start the engine") { _ in try engine.start(); return ["ok": true] }
OpenAppHost.method("open_plugin", "Show the instrument's UI") { p in
    let view = NSView(frame: .init(x: 0, y: 0, width: 480, height: 320))  // AU: AUAudioUnit.requestViewController
    return ["window": OpenAppHost.openWindow(title: "Synth", content: view)]
}
OpenAppHost.method("close_plugin") { p in OpenAppHost.closeWindow(p["window"] as? String ?? ""); return nil }
OpenAppHost.run()   // AppKit run loop + JSON-RPC; never returns
```
Views bind meters with `{ "$stream": "host:daw/meters", "path": "l" }` (html: `openapp.subscribe`). Audio stays on the
Mac; send state, not audio, over channels. `OpenAppHost.subscribe("daw/transport") { data in … }` receives view input.
Built-in method `openapp.windows` → `{ count, titles }`. A SwiftPM `host.package` must bring its own loop.

## Which audio path
- Voice note or dictation → `{ "host": "audio.record", "transcribe": "apple" }` (no code).
- Music app in an html view (recorder, metronome, synth, sampler, MIDI via Web MIDI): Web Audio plus `getUserMedia`
  for the microphone; the user allows the microphone once for the module. Fine for most apps; keep it simple first.
- Host module (`service: true`, AVAudioEngine / CoreMIDI / Audio Units): only for low latency, hardware MIDI devices,
  plugin hosting or audio that must keep running with no view open. Send meters and state over channels, not audio.

## Microphone and camera in html views
Call `openapp.requestMedia({ audio: true })` (or `video: true`, or both) from a user action. The first call per module
and kind shows a consent card (Allow / Deny, remembered in grants `microphone:<module>` / `camera:<module>`); then it
returns the `MediaStream`. Plain `navigator.mediaDevices.getUserMedia` does the same (the bridge asks first). The kernel
allows capture only for this view's `oamod://<module>` origin with the grant: a denied grant, or a site embedded in the
view, gets `NotAllowedError`. Show the error and offer a retry; the user can revoke the grant in the grants panel.

```js
document.querySelector("#rec").onclick = async () => {
  try {
    const stream = await openapp.requestMedia({ audio: true });
    const rec = new MediaRecorder(stream);
    rec.ondataavailable = (e) => chunks.push(e.data);
    rec.start();
  } catch (e) { status.textContent = "Microphone not allowed: " + e.message; }
};
```
