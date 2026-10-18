import AppKit
import Foundation

/// Test helper (used by examples/macos_smoke.rs): draw text into a PNG with CoreGraphics/AppKit.
enum DebugService {
    static func renderText(_ p: [String: JSON]) throws -> JSON {
        let url = try fileURL(p, mustExist: false)
        let text = try p.need("text")
        let size = CGSize(width: 1200, height: 300)
        guard let ctx = CGContext(data: nil, width: Int(size.width), height: Int(size.height), bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
            throw RPCError(-32000, "no bitmap context")
        }
        ctx.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
        ctx.fill(CGRect(origin: .zero, size: size))
        let ns = NSGraphicsContext(cgContext: ctx, flipped: false)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ns
        let attrs: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: 72, weight: .semibold), .foregroundColor: NSColor.black]
        NSAttributedString(string: text, attributes: attrs).draw(at: CGPoint(x: 40, y: 110))
        NSGraphicsContext.restoreGraphicsState()
        guard let img = ctx.makeImage() else { throw RPCError(-32000, "no image") }
        let rep = NSBitmapImageRep(cgImage: img)
        guard let png = rep.representation(using: .png, properties: [:]) else { throw RPCError(-32000, "png encoding failed") }
        try png.write(to: url)
        return .object(["path": .string(url.path), "bytes": .int(png.count)])
    }
}
