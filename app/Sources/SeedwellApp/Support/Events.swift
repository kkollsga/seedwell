import Foundation
import UserNotifications
import os

/// One push event from the core, hopped off the core's thread.
enum CoreEvent: Sendable {
    case finished(TransferRow)
    case error(id: String?, message: String)
    case goalReached(TransferRow)
    case needsChoice(TransferRow)
    case autoAdded(AutoAdded)
    case duplicate(id: String, name: String, trackersAdded: Int)
    case addRequest(AddRequestInfo)
    case agent(AgentActivity)
    case settingsChanged(file: String, warnings: [String], restartRequired: [String])
    case settingsRejected(file: String, error: String)
    case locationsChanged(torrents: UInt32, bytes: UInt64)
    case attention
    case pending
}

/// Receives the core's rare push events on a core thread and forwards them as values.
final class CoreEvents: CoreListener, Sendable {
    private let send: @Sendable (CoreEvent) -> Void
    init(_ send: @escaping @Sendable (CoreEvent) -> Void) { self.send = send }

    func onTransferFinished(row: TransferRow) { send(.finished(row)) }
    func onError(id: String?, message: String) { send(.error(id: id, message: message)) }
    func onGoalReached(row: TransferRow) { send(.goalReached(row)) }
    func onNeedsChoice(row: TransferRow) { send(.needsChoice(row)) }
    func onAutoAdded(added: AutoAdded) { send(.autoAdded(added)) }
    func onDuplicate(id: String, name: String, trackersAdded: UInt32) {
        send(.duplicate(id: id, name: name, trackersAdded: Int(trackersAdded)))
    }
    func onAddRequest(request: AddRequestInfo) { send(.addRequest(request)) }
    func onAgentActivity(activity: AgentActivity) { send(.agent(activity)) }
    func onSettingsChanged(file: String, warnings: [String], restartRequired: [String]) {
        send(.settingsChanged(file: file, warnings: warnings, restartRequired: restartRequired))
    }
    func onSettingsRejected(file: String, error: String) { send(.settingsRejected(file: file, error: error)) }
    func onLocationsChanged(torrents: UInt32, bytes: UInt64) { send(.locationsChanged(torrents: torrents, bytes: bytes)) }
    func onAttentionChanged() { send(.attention) }
    func onPendingChanged() { send(.pending) }
}

/// System notifications (SPEC §9): posts what the core's `Notices` decided; a click gives the notice's `click` back to
/// the core (`noticeRoute`), which says what to open.
@MainActor enum Notifier {
    /// Notification centre needs a real bundle; skipped under `swift run`. A quiet QA run posts none (QuietRun).
    static var available: Bool { Bundle.main.bundleIdentifier != nil && Bundle.main.bundlePath.hasSuffix(".app") && !QuietRun.on }
    private static let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "notify")
    /// The `userInfo` key of a notice's `click`.
    nonisolated static let clickKey = "click"

    static func requestAuthorization() {
        guard available else { return }
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
        registerCategories()
    }

    /// The notifications with buttons (an agent's request: Approve, Deny), as the core lists them.
    static func registerCategories() {
        let categories = noticeCategories().map { c in
            UNNotificationCategory(identifier: c.id, actions: c.actions.map { a in
                var options: UNNotificationActionOptions = []
                if a.destructive { options.insert(.destructive) }
                if a.authenticate { options.insert(.authenticationRequired) }
                return UNNotificationAction(identifier: a.id, title: a.title, options: options)
            }, intentIdentifiers: [], options: [])
        }
        UNUserNotificationCenter.current().setNotificationCategories(Set(categories))
    }

    /// A notice of the core's; one with a `key` takes the place of the last one with that key.
    static func post(_ n: Notice) {
        post(title: n.title, body: n.body, key: n.key ?? UUID().uuidString, click: n.click, category: n.category)
    }

    /// A notification whose click opens the window, unless `click` (a `Notice.click`) says otherwise; `category`: its
    /// buttons (`registerCategories`).
    static func post(title: String, body: String, key: String = UUID().uuidString, click: String = "", category: String? = nil) {
        guard available else { log.notice("(notification) \(title, privacy: .public): \(body, privacy: .private)"); return }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.userInfo = [clickKey: click]
        if let category { content.categoryIdentifier = category }
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: key, content: content, trigger: nil))
    }

    /// `/Users/me/Movies` as `~/Movies`; `$HOME` is the one the app runs with (`--home-root` aware).
    static func abbreviate(_ path: String) -> String {
        guard let home = getenv("HOME").map({ String(cString: $0) }), !home.isEmpty, home != "/" else { return path }
        if path == home { return "~" }
        return path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
    }
}
