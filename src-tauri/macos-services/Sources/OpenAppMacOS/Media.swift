@preconcurrency import AVFoundation
import Foundation
import NaturalLanguage
import Speech
import Translation
import Vision

// MARK: - Speech (SpeechAnalyzer + SpeechTranscriber, on-device)

enum SpeechService {
    static func status() async -> JSON {
        .object([
            "available": .bool(SpeechTranscriber.isAvailable),
            "installed_locales": .int(await SpeechTranscriber.installedLocales.count),
        ])
    }

    static func transcribe(_ p: [String: JSON]) async throws -> JSON {
        let url = try fileURL(p)
        guard SpeechTranscriber.isAvailable else { throw RPCError.unavailable("on-device speech transcription is not available on this Mac") }
        let file: AVAudioFile
        do {
            file = try AVAudioFile(forReading: url)
        } catch {
            throw RPCError(-32010, "macOS cannot decode \(url.pathExtension.isEmpty ? "this file" : "a .\(url.pathExtension) file") (\(error.localizedDescription)). Record audio/mp4 (AAC, .m4a) or WAV.")
        }
        let wanted = p.str("locale").map { Locale(identifier: $0) } ?? Locale.current
        var locale = await SpeechTranscriber.supportedLocale(equivalentTo: wanted)
        if locale == nil && p.str("locale") == nil {
            locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: "en-US"))
        }
        guard let locale else { throw RPCError.invalid("locale \(wanted.identifier) is not supported by on-device transcription") }
        let transcriber = SpeechTranscriber(locale: locale, transcriptionOptions: [], reportingOptions: [], attributeOptions: [.audioTimeRange])
        var installed = false
        if let req = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
            log("installing speech assets for \(locale.identifier)")
            try await req.downloadAndInstall()
            installed = true
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        let collector = Task { () throws -> [(String, Double, Double)] in
            var segs: [(String, Double, Double)] = []
            for try await r in transcriber.results {
                let text = String(r.text.characters).trimmingCharacters(in: .whitespacesAndNewlines)
                if !text.isEmpty { segs.append((text, r.range.start.seconds, r.range.end.seconds)) }
            }
            return segs
        }
        if let last = try await analyzer.analyzeSequence(from: file) {
            try await analyzer.finalizeAndFinish(through: last)
        } else {
            await analyzer.cancelAndFinishNow()
        }
        let segs = try await collector.value
        return .object([
            "text": .string(segs.map(\.0).joined(separator: " ")),
            "segments": .array(segs.map { .object(["text": .string($0.0), "start": .double($0.1), "end": .double($0.2)]) }),
            "locale": .string(locale.identifier),
            "duration": .double(Double(file.length) / file.processingFormat.sampleRate),
            "installed_assets": .bool(installed),
        ])
    }
}

// MARK: - Vision OCR

enum VisionService {
    static func ocr(_ p: [String: JSON]) throws -> JSON {
        let url = try fileURL(p)
        let req = VNRecognizeTextRequest()
        req.recognitionLevel = .accurate
        req.usesLanguageCorrection = true
        if let langs = p["languages"]?.array?.compactMap(\.string), !langs.isEmpty {
            req.recognitionLanguages = langs
        } else {
            req.automaticallyDetectsLanguage = true
        }
        let handler = VNImageRequestHandler(url: url)
        do { try handler.perform([req]) } catch {
            throw RPCError(-32011, "cannot read the image: \(error.localizedDescription)")
        }
        var lines: [JSON] = []
        for o in req.results ?? [] {
            guard let c = o.topCandidates(1).first else { continue }
            let b = o.boundingBox  // normalized, origin bottom-left
            lines.append(.object([
                "text": .string(c.string),
                "confidence": .double(Double(c.confidence)),
                "box": .object(["x": .double(b.origin.x), "y": .double(1 - b.origin.y - b.height), "width": .double(b.width), "height": .double(b.height)]),
            ]))
        }
        return .object(["text": .string(lines.compactMap { $0["text"]?.string }.joined(separator: "\n")), "lines": .array(lines)])
    }
}

// MARK: - Text to speech (AVSpeechSynthesizer)

final class Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func with<R>(_ f: (inout T) -> R) -> R {
        lock.lock()
        defer { lock.unlock() }
        return f(&value)
    }
}

@MainActor
enum TTS {
    static func utterance(_ p: [String: JSON]) throws -> AVSpeechUtterance {
        let u = AVSpeechUtterance(string: try p.need("text"))
        if let v = p.str("voice") {
            guard let voice = AVSpeechSynthesisVoice(identifier: v) ?? AVSpeechSynthesisVoice(language: v) else {
                throw RPCError.invalid("unknown voice \"\(v)\" (use a voice identifier or a language like en-US)")
            }
            u.voice = voice
        } else {
            u.voice = AVSpeechSynthesisVoice(language: AVSpeechSynthesisVoice.currentLanguageCode())
        }
        if let r = p["rate"]?.double { u.rate = Float(max(0, min(1, r))) }
        return u
    }

    final class Waiter: NSObject, AVSpeechSynthesizerDelegate, @unchecked Sendable {
        let done = Box<CheckedContinuation<Void, Never>?>(nil)
        func finish() { done.with { c in c?.resume(); c = nil } }
        func speechSynthesizer(_ s: AVSpeechSynthesizer, didFinish u: AVSpeechUtterance) { finish() }
        func speechSynthesizer(_ s: AVSpeechSynthesizer, didCancel u: AVSpeechUtterance) { finish() }
    }

    static func speak(_ p: [String: JSON]) async throws -> JSON {
        let u = try utterance(p)
        let synth = AVSpeechSynthesizer()
        let w = Waiter()
        synth.delegate = w
        let t0 = Date()
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            w.done.with { $0 = c }
            synth.speak(u)
        }
        _ = synth
        return .object(["seconds": .double(Date().timeIntervalSince(t0))])
    }

    /// Render speech to a file: .m4a (AAC), .caf, .wav or .aiff (PCM).
    static func render(_ p: [String: JSON]) async throws -> JSON {
        let url = try fileURL(p, mustExist: false)
        let ext = url.pathExtension.lowercased()
        guard ["m4a", "caf", "wav", "aiff", "aif"].contains(ext) else {
            throw RPCError.invalid("\"path\" must end in .m4a, .caf, .wav or .aiff")
        }
        try? FileManager.default.removeItem(at: url)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let u = try utterance(p)
        let synth = AVSpeechSynthesizer()
        let file = Box<AVAudioFile?>(nil)
        let failure = Box<String?>(nil)
        let frames = Box<Int64>(0)
        let finished = Box<Bool>(false)
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            synth.write(u) { buffer in
                guard let pcm = buffer as? AVAudioPCMBuffer else { return }
                if pcm.frameLength == 0 {
                    if finished.with({ f in let was = f; f = true; return was }) == false { c.resume() }
                    return
                }
                file.with { f in
                    do {
                        if f == nil {
                            let fmt = pcm.format
                            var settings = fmt.settings
                            if ext == "m4a" {
                                settings = [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: fmt.sampleRate,
                                            AVNumberOfChannelsKey: fmt.channelCount, AVEncoderBitRateKey: 64000]
                            } else if ext == "wav" || ext == "aiff" || ext == "aif" {
                                settings = [AVFormatIDKey: kAudioFormatLinearPCM, AVSampleRateKey: fmt.sampleRate,
                                            AVNumberOfChannelsKey: fmt.channelCount, AVLinearPCMBitDepthKey: 16,
                                            AVLinearPCMIsFloatKey: false, AVLinearPCMIsBigEndianKey: ext != "wav"]
                            }
                            f = try AVAudioFile(forWriting: url, settings: settings, commonFormat: fmt.commonFormat, interleaved: fmt.isInterleaved)
                        }
                        try f?.write(from: pcm)
                        frames.with { $0 += Int64(pcm.frameLength) }
                    } catch {
                        failure.with { $0 = error.localizedDescription }
                    }
                }
            }
        }
        _ = synth
        let rate = file.with { f -> Double in
            let r = f?.fileFormat.sampleRate ?? 0
            f = nil  // closes and flushes the file
            return r
        }
        if let e = failure.with({ $0 }) { throw RPCError(-32012, "could not write \(url.lastPathComponent): \(e)") }
        let n = frames.with { $0 }
        guard n > 0 else { throw RPCError(-32012, "the synthesizer produced no audio") }
        return .object(["path": .string(url.path), "seconds": .double(rate > 0 ? Double(n) / rate : 0)])
    }
}

// MARK: - NaturalLanguage

enum NL {
    static func embed(_ p: [String: JSON]) throws -> JSON {
        guard let texts = p["texts"]?.array?.compactMap(\.string), !texts.isEmpty else {
            throw RPCError.invalid("\"texts\" (array of strings) is required")
        }
        var lang = p.str("language").map { NLLanguage(rawValue: $0) }
        if lang == nil {
            let r = NLLanguageRecognizer()
            r.processString(texts.joined(separator: "\n"))
            lang = r.dominantLanguage
        }
        var used = lang ?? .english
        var emb = NLEmbedding.sentenceEmbedding(for: used)
        if emb == nil {
            used = .english
            emb = NLEmbedding.sentenceEmbedding(for: .english)
        }
        guard let emb else { throw RPCError.unavailable("no sentence embedding model on this Mac") }
        let vectors: [JSON] = texts.map { t in .array((emb.vector(for: t) ?? []).map { .double($0) }) }
        return .object(["vectors": .array(vectors), "dimension": .int(emb.dimension), "language": .string(used.rawValue)])
    }

    static func language(_ p: [String: JSON]) throws -> JSON {
        let text = try p.need("text")
        let r = NLLanguageRecognizer()
        r.processString(text)
        let hyp = r.languageHypotheses(withMaximum: 3)
        let best = r.dominantLanguage
        return .object([
            "language": best.map { .string($0.rawValue) } ?? .null,
            "confidence": .double(best.flatMap { hyp[$0] } ?? 0),
            "alternatives": .object(Dictionary(uniqueKeysWithValues: hyp.map { ($0.key.rawValue, JSON.double($0.value)) })),
        ])
    }
}

// MARK: - Translation (non-UI session, installed languages only)

enum Translate {
    static func status() async -> JSON {
        .object(["available": .bool(true), "note": .string("only language pairs installed in System Settings › General › Language & Region › Translation Languages")])
    }

    static func translate(_ p: [String: JSON]) async throws -> JSON {
        let text = try p.need("text")
        let to = try p.need("to")
        var from = p.str("from")
        if from == nil {
            let r = NLLanguageRecognizer()
            r.processString(text)
            from = r.dominantLanguage?.rawValue
        }
        guard let from else { throw RPCError.invalid("could not detect the source language; pass \"from\"") }
        let src = Locale.Language(identifier: from)
        let dst = Locale.Language(identifier: to)
        let st = await LanguageAvailability().status(from: src, to: dst)
        switch st {
        case .installed: break
        case .supported:
            throw RPCError.unavailable("translation \(from) → \(to) is supported but not installed; the user can install it in System Settings › General › Language & Region › Translation Languages")
        case .unsupported:
            throw RPCError.invalid("translation \(from) → \(to) is not supported")
        @unknown default:
            throw RPCError.unavailable("translation \(from) → \(to): unknown availability")
        }
        let session = TranslationSession(installedSource: src, target: dst)
        let r = try await session.translate(text)
        return .object(["text": .string(r.targetText), "from": .string(from), "to": .string(to)])
    }
}
