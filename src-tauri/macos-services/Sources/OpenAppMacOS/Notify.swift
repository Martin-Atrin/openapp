import AppKit
import Foundation
import UserNotifications

/// §21 G4 notifications with actions (UserNotifications). Works only when this binary runs from an app bundle
/// (build.rs wraps it as `OpenApp Notifications.app`); a bare binary answers `available: false`.
/// Answers arrive as JSON-RPC notifications `notify.response { id, action }` on stdout
/// (`action` = an action id, `default` for a click on the notification, `dismiss`).
enum NotifyService {
    static var bundled: Bool {
        Bundle.main.bundleIdentifier != nil && Bundle.main.bundleURL.pathExtension == "app"
    }

    /// Call once at start (before any notification can be answered).
    @MainActor static func setup() {
        guard bundled else { return }
        // a UI-element app (no Dock icon) so the system treats it as an application for notifications
        _ = NSApplication.shared
        NSApp.setActivationPolicy(.accessory)
        UNUserNotificationCenter.current().delegate = NotifyDelegate.shared
    }

    static func status() async -> JSON {
        guard bundled else { return .object(["available": .bool(false), "reason": .string("not running from an app bundle")]) }
        let s = await UNUserNotificationCenter.current().notificationSettings()
        let auth: String
        switch s.authorizationStatus {
        case .authorized: auth = "authorized"
        case .denied: auth = "denied"
        case .provisional: auth = "provisional"
        case .ephemeral: auth = "ephemeral"
        default: auth = "not_determined"
        }
        return .object(["available": .bool(true), "authorization": .string(auth)])
    }

    static func post(_ p: [String: JSON]) async throws -> JSON {
        guard bundled else { throw RPCError.unavailable("notifications need the bundled helper") }
        let center = UNUserNotificationCenter.current()
        var reason = ""
        var granted = false
        do {
            granted = try await center.requestAuthorization(options: [.alert, .sound, .badge])
        } catch {
            reason = " (\(error.localizedDescription))"
        }
        if !granted {
            throw RPCError(-32002, "notifications are not allowed for OpenApp (System Settings › Notifications)" + reason)
        }
        let id = p.str("id") ?? UUID().uuidString
        let content = UNMutableNotificationContent()
        content.title = p.str("title") ?? ""
        content.body = p.str("body") ?? ""
        if let sub = p.str("subtitle") { content.subtitle = sub }
        if p["sound"]?.bool ?? true { content.sound = .default }
        let actions: [ActionSpec] = (p["actions"]?.array ?? []).compactMap { a in
            guard let aid = a["id"]?.string, let label = a["label"]?.string else { return nil }
            return ActionSpec(id: aid, label: label)
        }
        if !actions.isEmpty {
            let cat = "oa." + actions.map { $0.id + "=" + $0.label }.joined(separator: "|")
            await Categories.shared.add(cat, actions)
            content.categoryIdentifier = cat
        }
        let req = UNNotificationRequest(identifier: id, content: content, trigger: nil)
        try await center.add(req)
        return .object(["id": .string(id), "delivered": .bool(true), "actions": .int(actions.count)])
    }
}

struct ActionSpec: Sendable {
    let id: String
    let label: String
}

/// Registered categories (setNotificationCategories replaces the whole set).
actor Categories {
    static let shared = Categories()
    private var all: [String: [ActionSpec]] = [:]
    func add(_ id: String, _ actions: [ActionSpec]) async {
        if all[id] != nil { return }
        all[id] = actions
        let cats = all.map { (cid, acts) in
            UNNotificationCategory(identifier: cid, actions: acts.map { UNNotificationAction(identifier: $0.id, title: $0.label, options: [.foreground]) },
                                   intentIdentifiers: [], options: [.customDismissAction])
        }
        UNUserNotificationCenter.current().setNotificationCategories(Set(cats))
        // the system applies categories asynchronously; give it a moment before the first post
        try? await Task.sleep(for: .milliseconds(150))
    }
}

final class NotifyDelegate: NSObject, UNUserNotificationCenterDelegate, Sendable {
    static let shared = NotifyDelegate()

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        var action = response.actionIdentifier
        if action == UNNotificationDefaultActionIdentifier { action = "default" }
        if action == UNNotificationDismissActionIdentifier { action = "dismiss" }
        let id = response.notification.request.identifier
        await Output.shared.send(.object(["jsonrpc": .string("2.0"), "method": .string("notify.response"),
                                          "params": .object(["id": .string(id), "action": .string(action)])]))
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }
}
