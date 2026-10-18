import Foundation
import FoundationModels

/// FoundationModels (Apple Intelligence on-device model).
enum AI {
    static func status() -> JSON {
        switch SystemLanguageModel.default.availability {
        case .available:
            return .object(["available": .bool(true)])
        case .unavailable(let reason):
            return .object(["available": .bool(false), "reason": .string(String(describing: reason))])
        }
    }

    struct Unsupported: Error { let why: String }

    static func generate(_ p: [String: JSON]) async throws -> JSON {
        guard case .available = SystemLanguageModel.default.availability else {
            throw RPCError.unavailable("Apple Intelligence model unavailable: \(status()["reason"]?.string ?? "unknown")")
        }
        let prompt = try p.need("prompt")
        let session = LanguageModelSession(instructions: p.str("instructions"))
        var options = GenerationOptions()
        if let t = p["temperature"]?.double { options.temperature = t }
        if let m = p["max_tokens"]?.int ?? p["maxTokens"]?.int { options.maximumResponseTokens = m }
        guard let schema = p["schema"], !schema.isNull else {
            let r = try await mapErrors { try await session.respond(to: prompt, options: options) }
            return .object(["text": .string(r.content)])
        }
        // guided generation from the JSON Schema; fall back to "respond with JSON" + validation
        do {
            var deps: [DynamicGenerationSchema] = []
            let root = try dynamic(schema, name: "Root", deps: &deps)
            let gs = try GenerationSchema(root: root, dependencies: deps)
            let r = try await mapErrors { try await session.respond(to: prompt, schema: gs, options: options) }
            return .object(["json": try JSON.parse(r.content.jsonString), "mode": .string("guided")])
        } catch let u as Unsupported {
            log("schema not supported for guided generation (\(u.why)); using the JSON fallback")
        }
        let fallbackPrompt = """
        \(prompt)

        Respond with only one JSON value (no prose, no code fences) that matches this JSON Schema:
        \(schema.compact)
        """
        let r = try await mapErrors { try await session.respond(to: fallbackPrompt, options: options) }
        var text = r.content.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.hasPrefix("```") {
            text = text.components(separatedBy: "\n").dropFirst().joined(separator: "\n")
            if let i = text.range(of: "```", options: .backwards) { text = String(text[..<i.lowerBound]) }
        }
        guard let json = try? JSON.parse(text) else {
            throw RPCError(-32002, "the model did not return valid JSON")
        }
        if let problem = validate(json, schema, at: "$") {
            throw RPCError(-32002, "the model's JSON does not match the schema: \(problem)")
        }
        return .object(["json": json, "mode": .string("json-fallback")])
    }

    private static func mapErrors<T>(_ op: () async throws -> T) async throws -> T {
        do { return try await op() } catch let e as LanguageModelSession.GenerationError {
            throw RPCError(-32003, "generation failed: \(e.localizedDescription) (\(String(describing: e)))")
        }
    }

    /// JSON Schema (object/string/number/integer/boolean/array/enum) → DynamicGenerationSchema.
    static func dynamic(_ s: JSON, name: String, deps: inout [DynamicGenerationSchema]) throws -> DynamicGenerationSchema {
        guard let o = s.object else { throw Unsupported(why: "schema at \(name) is not an object") }
        for k in ["$ref", "oneOf", "anyOf", "allOf", "not", "patternProperties"] where o[k] != nil {
            throw Unsupported(why: "\(k) at \(name)")
        }
        let desc = o["description"]?.string
        if let e = o["enum"]?.array {
            let strs = e.compactMap(\.string)
            guard strs.count == e.count, !strs.isEmpty else { throw Unsupported(why: "non-string enum at \(name)") }
            return DynamicGenerationSchema(name: name, description: desc, anyOf: strs)
        }
        var type = o["type"]?.string
        if type == nil, case .array(let ts)? = o["type"] {
            let nonNull = ts.compactMap(\.string).filter { $0 != "null" }
            if nonNull.count == 1 { type = nonNull[0] }
        }
        if type == nil && o["properties"] != nil { type = "object" }
        switch type {
        case "object":
            let props = o["properties"]?.object ?? [:]
            let required = Set((o["required"]?.array ?? []).compactMap(\.string))
            var out: [DynamicGenerationSchema.Property] = []
            for key in props.keys.sorted() {
                let sub = props[key]!
                let child = try dynamic(sub, name: "\(name)_\(key)", deps: &deps)
                out.append(.init(name: key, description: sub["description"]?.string, schema: child, isOptional: !required.contains(key)))
            }
            return DynamicGenerationSchema(name: name, description: desc, properties: out)
        case "string": return DynamicGenerationSchema(type: String.self)
        case "integer": return DynamicGenerationSchema(type: Int.self)
        case "number": return DynamicGenerationSchema(type: Double.self)
        case "boolean": return DynamicGenerationSchema(type: Bool.self)
        case "array":
            guard let items = o["items"] else { throw Unsupported(why: "array without items at \(name)") }
            let item = try dynamic(items, name: "\(name)_item", deps: &deps)
            return DynamicGenerationSchema(arrayOf: item, minimumElements: o["minItems"]?.int, maximumElements: o["maxItems"]?.int)
        default:
            throw Unsupported(why: "type \(type ?? "missing") at \(name)")
        }
    }

    /// Minimal JSON Schema check for the fallback path (type, required, enum, items).
    static func validate(_ v: JSON, _ s: JSON, at path: String) -> String? {
        guard let o = s.object else { return nil }
        if let e = o["enum"]?.array, !e.contains(v) { return "\(path): not one of the enum values" }
        let t = o["type"]?.string ?? (o["properties"] != nil ? "object" : nil)
        switch t {
        case "object":
            guard let obj = v.object else { return "\(path): expected an object" }
            for r in (o["required"]?.array ?? []).compactMap(\.string) where obj[r] == nil { return "\(path).\(r): missing" }
            for (k, sub) in o["properties"]?.object ?? [:] {
                if let x = obj[k], let p = validate(x, sub, at: "\(path).\(k)") { return p }
            }
        case "array":
            guard let a = v.array else { return "\(path): expected an array" }
            if let items = o["items"] {
                for (i, x) in a.enumerated() { if let p = validate(x, items, at: "\(path)[\(i)]") { return p } }
            }
        case "string": if v.string == nil { return "\(path): expected a string" }
        case "integer": if v.int == nil { return "\(path): expected an integer" }
        case "number": if v.double == nil { return "\(path): expected a number" }
        case "boolean": if v.bool == nil { return "\(path): expected a boolean" }
        default: break
        }
        return nil
    }
}
