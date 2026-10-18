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
    case duplicate(String, trackersAdded: Int)
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
    func onDuplicate(name: String, trackersAdded: UInt32) { send(.duplicate(name, trackersAdded: Int(trackersAdded))) }
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

/// System notifications (SPEC §9). Clicking one opens the main window on the transfer.
@MainActor enum Notifier {
    /// Notification centre needs a real bundle; skipped under `swift run`.
    static var available: Bool { Bundle.main.bundleIdentifier != nil && Bundle.main.bundlePath.hasSuffix(".app") }
    private static let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "notify")

    static func requestAuthorization() {
        guard available else { return }
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    static func post(title: String, body: String, transfer: String? = nil, key: String = UUID().uuidString) {
        guard available else { log.notice("(notification) \(title, privacy: .public): \(body, privacy: .public)"); return }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        if let transfer { content.userInfo = ["transfer": transfer] }
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: key, content: content, trigger: nil))
    }

    private static var burst: [Date] = []

    /// More than three in ten seconds become one "N downloads finished" that replaces itself.
    static func finished(_ row: TransferRow) {
        let now = Date()
        burst = burst.filter { now.timeIntervalSince($0) < 10 } + [now]
        if burst.count <= 3 {
            post(title: "Download finished", body: row.displayName, transfer: row.id)
        } else {
            post(title: "\(burst.count) downloads finished", body: "Open \(AppInfo.name) to see them.", key: "finished-burst")
        }
    }

    static func failed(_ name: String, message: String, id: String?) {
        post(title: "Problem with \(name)", body: message, transfer: id)
    }

    /// SPEC §7A.1: "Added Night of the Living Dead (1968) · Movie · tracker-y.net → ~/Media/Movies".
    static func autoAdded(_ a: AutoAdded) {
        var parts = [a.category.label, a.tracker]
        if let d = a.destination { parts.append("→ " + abbreviate(d)) }
        post(title: "Added \(a.name)", body: parts.joined(separator: " · "), transfer: a.id)
    }

    static func needsChoice(_ row: TransferRow) {
        post(title: "Choose what to download", body: row.displayName, transfer: row.id, key: "choice-\(row.id)")
    }

    static func duplicate(_ name: String, trackersAdded: Int = 0) {
        if trackersAdded > 0 {
            post(title: "Added \(trackersAdded) tracker\(trackersAdded == 1 ? "" : "s") to \(name)", body: "It was already in \(AppInfo.name)")
            return
        }
        post(title: "Already in \(AppInfo.name)", body: name)
    }

    static func goalReached(_ name: String) {
        post(title: "Seeding goal reached", body: name)
    }

    static func agent(_ a: AgentActivity) {
        guard a.outcome == "ok" else { return }
        post(title: "\(a.client)", body: a.summary)
    }

    /// `/Users/me/Movies` as `~/Movies`; `$HOME` is the one the app runs with (`--home-root` aware).
    static func abbreviate(_ path: String) -> String {
        guard let home = getenv("HOME").map({ String(cString: $0) }), !home.isEmpty, home != "/" else { return path }
        if path == home { return "~" }
        return path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
    }
}
