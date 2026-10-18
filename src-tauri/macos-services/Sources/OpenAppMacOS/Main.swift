import Foundation

/// openapp-macos: JSON-RPC 2.0 over stdio (one JSON object per line). Requests run concurrently;
/// requests to the same method run one at a time. Logs go to stderr; stdout carries only JSON-RPC.
@main
struct Main {
    static func main() async {
        let out = Output.shared
        let serial = Serial()
        await MainActor.run { NotifyService.setup() }
        do {
            for try await line in FileHandle.standardInput.bytes.lines {
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                if trimmed.isEmpty { continue }
                guard let msg = try? JSON.parse(trimmed), let obj = msg.object else {
                    await out.send(.object(["jsonrpc": .string("2.0"), "id": .null,
                                            "error": .object(["code": .int(-32700), "message": .string("parse error")])]))
                    continue
                }
                let id = obj["id"] ?? .null
                let method = obj["method"]?.string ?? ""
                let params = obj["params"]?.object ?? [:]
                await serial.run(method) {
                    let reply: JSON
                    do {
                        let result = try await Services.dispatch(method, params)
                        reply = .object(["jsonrpc": .string("2.0"), "id": id, "result": result])
                    } catch let e as RPCError {
                        reply = .object(["jsonrpc": .string("2.0"), "id": id,
                                         "error": .object(["code": .int(e.code), "message": .string(e.message)])])
                    } catch {
                        reply = .object(["jsonrpc": .string("2.0"), "id": id,
                                         "error": .object(["code": .int(-32000), "message": .string(String(describing: error))])])
                    }
                    if !id.isNull { await out.send(reply) }
                }
            }
        } catch {
            FileHandle.standardError.write(Data("openapp-macos: stdin error: \(error)\n".utf8))
        }
        // stdin closed: let running requests finish briefly, then exit
        try? await Task.sleep(for: .milliseconds(200))
        exit(0)
    }
}

actor Output {
    static let shared = Output()
    func send(_ v: JSON) {
        var data = Data(v.compact.utf8)
        data.append(0x0A)
        FileHandle.standardOutput.write(data)
    }
}

/// Runs operations with the same key one after another.
actor Serial {
    private var tails: [String: Task<Void, Never>] = [:]
    func run(_ key: String, _ op: @escaping @Sendable () async -> Void) {
        let prev = tails[key]
        tails[key] = Task {
            await prev?.value
            await op()
        }
    }
}

enum Services {
    static let methods = ["status", "ai.status", "ai.generate", "speech.transcribe", "vision.ocr", "tts.speak",
                          "tts.render", "translate", "nl.embed", "nl.language", "notify.post", "notify.status"]

    static func dispatch(_ method: String, _ p: [String: JSON]) async throws -> JSON {
        switch method {
        case "status": return await status()
        case "ai.status": return AI.status()
        case "ai.generate": return try await AI.generate(p)
        case "speech.transcribe": return try await SpeechService.transcribe(p)
        case "vision.ocr": return try VisionService.ocr(p)
        case "tts.speak": return try await TTS.speak(p)
        case "tts.render": return try await TTS.render(p)
        case "translate": return try await Translate.translate(p)
        case "nl.embed": return try NL.embed(p)
        case "nl.language": return try NL.language(p)
        case "notify.post": return try await NotifyService.post(p)
        case "notify.status": return await NotifyService.status()
        case "debug.render_text": return try await MainActor.run { try DebugService.renderText(p) }
        default: throw RPCError(-32601, "unknown method \"\(method)\" (methods: \(methods.joined(separator: ", ")))")
        }
    }

    /// Availability of every service (for describe → host_services).
    static func status() async -> JSON {
        .object([
            "version": .string("1"),
            "methods": .array(methods.map { .string($0) }),
            "ai": AI.status(),
            "speech": await SpeechService.status(),
            "vision": .object(["available": .bool(true)]),
            "tts": .object(["available": .bool(true)]),
            "nl": .object(["available": .bool(true)]),
            "translate": await Translate.status(),
        ])
    }
}

func log(_ s: String) {
    FileHandle.standardError.write(Data("openapp-macos: \(s)\n".utf8))
}

/// Path must be absolute (the kernel maps workspace paths to host paths before calling).
func fileURL(_ p: [String: JSON], _ key: String = "path", mustExist: Bool = true) throws -> URL {
    let path = try p.need(key)
    guard path.hasPrefix("/") else { throw RPCError.invalid("\"\(key)\" must be an absolute path") }
    if mustExist && !FileManager.default.fileExists(atPath: path) {
        throw RPCError.invalid("file not found: \(path)")
    }
    return URL(fileURLWithPath: path)
}
