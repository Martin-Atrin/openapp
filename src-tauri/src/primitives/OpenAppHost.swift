// OpenApp host-module runtime (kernel-provided, contract §21 G8). The kernel compiles this file next to a host
// module's own Swift sources when they use `OpenAppHost` (single-file / folder builds). It speaks the host-module
// JSON-RPC 2.0 (one JSON object per line) on a background thread and runs every handler on the main thread, so
// AppKit works: the process is an NSApplication accessory app (no Dock icon) that may open its own windows
// (e.g. Audio Unit plugin UIs), keep an AVAudioEngine running (`"service": true`) and stream over live channels.
//
//   OpenAppHost.method("play", "Start playback") { p in engine.start(); return ["ok": true] }
//   OpenAppHost.publish("host:daw/meters", ["l": 0.4, "r": 0.5])
//   OpenAppHost.subscribe("daw/transport") { data in ... }
//   let id = OpenAppHost.openWindow(title: "Synth", width: 480, height: 320, content: view)
//   OpenAppHost.run()            // never returns; exits when the kernel closes stdin
import AppKit
import Foundation

public enum OpenAppHost {
    public typealias Params = [String: Any]
    /// Handlers run on the main thread (AppKit is fine); they may be async and may throw.
    public typealias Handler = @MainActor (Params) async throws -> Any?

    nonisolated(unsafe) private static var handlers: [String: (description: String, fn: Handler)] = [:]
    nonisolated(unsafe) private static var order: [String] = []
    nonisolated(unsafe) private static var channelCallbacks: [String: [@MainActor (Any) -> Void]] = [:]
    @MainActor private static var windows: [String: NSWindow] = [:]
    @MainActor private static var nextWindow = 1
    private static let outLock = NSLock()
    private static let regLock = NSLock()

    /// Register a method (callable as `POST $OPENAPP_HOST_URL/<module>/<name>` / `host_call`). Any thread.
    public static func method(_ name: String, _ description: String = "", _ fn: @escaping Handler) {
        regLock.lock()
        if handlers[name] == nil { order.append(name) }
        handlers[name] = (description, fn)
        regLock.unlock()
    }

    /// Publish on a live channel (own topics: `host:<module>/<name>`), JSON-compatible data. Any thread.
    public static func publish(_ topic: String, _ data: Any) {
        notify("channel.publish", ["topic": topic, "data": data])
    }

    /// Subscribe to a topic (yours, `kernel/*`, or another module's public topic); `cb` gets the message data.
    public static func subscribe(_ topic: String, replay: Bool = false, _ cb: @escaping @MainActor (Any) -> Void) {
        regLock.lock()
        let first = channelCallbacks[topic] == nil
        channelCallbacks[topic, default: []].append(cb)
        regLock.unlock()
        if first { notify("channel.subscribe", ["topic": topic, "replay": replay]) }
    }

    /// Open a native window; returns its id. Close with `closeWindow`. `activate: false` shows it without taking focus.
    @discardableResult @MainActor
    public static func openWindow(title: String, width: Double = 480, height: Double = 320, content: NSView? = nil, activate: Bool = true) -> String {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: width, height: height),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = title
        w.isReleasedWhenClosed = false
        if let content { w.contentView = content }
        w.center()
        if activate {
            w.makeKeyAndOrderFront(nil)
            NSApp.activate()
        } else {
            w.orderFrontRegardless()
        }
        let id = "w\(nextWindow)"
        nextWindow += 1
        windows[id] = w
        return id
    }

    @MainActor public static func closeWindow(_ id: String) {
        windows[id]?.close()
        windows[id] = nil
    }

    /// Visible windows of this process (also the built-in method `openapp.windows`).
    @MainActor public static func visibleWindows() -> [String] {
        NSApp.windows.filter { $0.isVisible }.map { $0.title }
    }

    /// Start the JSON-RPC loop and the AppKit run loop (`appKit: false` = no NSApplication). Never returns.
    public static func run(appKit: Bool = true) -> Never {
        method("openapp.windows", "Visible native windows of this host module") { _ in
            let titles = visibleWindows()
            return ["count": titles.count, "titles": titles]
        }
        let reader = Thread {
            while let line = readLine() {
                DispatchQueue.main.async { MainActor.assumeIsolated { handle(line) } }
            }
            DispatchQueue.main.async { exit(0) }
        }
        reader.start()
        if appKit {
            // top-level code runs on the main thread
            MainActor.assumeIsolated {
                let app = NSApplication.shared
                app.setActivationPolicy(.accessory)
                app.run()
            }
        } else {
            dispatchMain()
        }
        exit(0)
    }

    // ---------- JSON-RPC ----------

    private static func send(_ msg: [String: Any]) {
        let obj = clean(msg)
        guard JSONSerialization.isValidJSONObject(obj), var data = try? JSONSerialization.data(withJSONObject: obj) else {
            FileHandle.standardError.write(Data("OpenAppHost: result is not JSON-serializable\n".utf8))
            return
        }
        data.append(0x0A)
        outLock.lock()
        FileHandle.standardOutput.write(data)
        outLock.unlock()
    }

    private static func notify(_ method: String, _ params: [String: Any]) {
        send(["jsonrpc": "2.0", "method": method, "params": params])
    }

    /// nil → NSNull, nested optionals and non-JSON values made safe.
    private static func clean(_ v: Any?) -> Any {
        guard let v else { return NSNull() }
        switch v {
        case let d as [String: Any]: return d.mapValues { clean($0) }
        case let a as [Any]: return a.map { clean($0) }
        case let s as String: return s
        case let n as NSNumber: return n
        case let b as Bool: return b
        case let i as Int: return i
        case let d as Double: return d.isFinite ? d : 0
        case let f as Float: return Double(f).isFinite ? Double(f) : 0
        case is NSNull: return NSNull()
        default: return String(describing: v)
        }
    }

    @MainActor private static func handle(_ line: String) {
        guard let obj = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any],
              let method = obj["method"] as? String else { return }
        let params = obj["params"] as? [String: Any] ?? [:]
        if method == "channel.message" {
            let topic = params["topic"] as? String ?? ""
            regLock.lock()
            let cbs = channelCallbacks[topic] ?? []
            regLock.unlock()
            for cb in cbs { cb(params["data"] ?? NSNull()) }
            return
        }
        guard let id = obj["id"] else { return }
        regLock.lock()
        let listing = order.map { ["name": $0, "description": handlers[$0]?.description ?? ""] }
        let h = handlers[method]
        regLock.unlock()
        if method == "methods" {
            send(["jsonrpc": "2.0", "id": id, "result": listing])
            return
        }
        guard let h else {
            send(["jsonrpc": "2.0", "id": id, "error": ["code": -32601, "message": "unknown method \(method)"]])
            return
        }
        Task { @MainActor in
            do {
                let r = try await h.fn(params)
                send(["jsonrpc": "2.0", "id": id, "result": r ?? NSNull()])
            } catch {
                send(["jsonrpc": "2.0", "id": id, "error": ["code": -32000, "message": error.localizedDescription]])
            }
        }
    }
}

/// Throw from a handler with a message: `throw HostError("not allowed")`.
public struct HostError: LocalizedError {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var errorDescription: String? { message }
}
