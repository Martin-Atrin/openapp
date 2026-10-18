import Foundation

/// A Sendable JSON value.
enum JSON: Sendable, Codable, Equatable {
    case null
    case bool(Bool)
    case int(Int)
    case double(Double)
    case string(String)
    case array([JSON])
    case object([String: JSON])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let i = try? c.decode(Int.self) { self = .int(i) }
        else if let d = try? c.decode(Double.self) { self = .double(d) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSON].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSON].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .int(let i): try c.encode(i)
        case .double(let d): try c.encode(d.isFinite ? d : 0)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    subscript(key: String) -> JSON? {
        if case .object(let o) = self { return o[key] }
        return nil
    }
    var string: String? { if case .string(let s) = self { return s }; return nil }
    var bool: Bool? { if case .bool(let b) = self { return b }; return nil }
    var int: Int? {
        switch self {
        case .int(let i): return i
        case .double(let d): return Int(exactly: d)
        default: return nil
        }
    }
    var double: Double? {
        switch self {
        case .int(let i): return Double(i)
        case .double(let d): return d
        default: return nil
        }
    }
    var array: [JSON]? { if case .array(let a) = self { return a }; return nil }
    var object: [String: JSON]? { if case .object(let o) = self { return o }; return nil }
    var isNull: Bool { self == .null }

    static func parse(_ s: String) throws -> JSON {
        try JSONDecoder().decode(JSON.self, from: Data(s.utf8))
    }

    var compact: String {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return (try? String(data: e.encode(self), encoding: .utf8)) ?? "null"
    }
}

/// A JSON-RPC error.
struct RPCError: Error, Sendable {
    let code: Int
    let message: String
    init(_ code: Int, _ message: String) {
        self.code = code
        self.message = message
    }
    static func invalid(_ m: String) -> RPCError { RPCError(-32602, m) }
    static func unavailable(_ m: String) -> RPCError { RPCError(-32001, m) }
}

extension Dictionary where Key == String, Value == JSON {
    func str(_ k: String) -> String? { self[k]?.string }
    func need(_ k: String) throws -> String {
        guard let v = self[k]?.string, !v.isEmpty else { throw RPCError.invalid("\"\(k)\" (string) is required") }
        return v
    }
}
