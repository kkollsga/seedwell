import Foundation

/// Agents' requests for access (SPEC 9A.3): the user's Approve and Deny, from a notification's button, the menu or
/// Settings › Agents. The core makes the change and says what happened (`agentRequestDecide`); this runs it off the
/// main thread and shows the answer. Nothing an agent runs reaches it.
@MainActor enum AgentRequests {
    /// Called after an answer: the menu and Settings read the requests again.
    static var answered: () -> Void = {}

    /// `notify`: say what happened in a notification (the answer came from one); otherwise only a failure is said (the
    /// row going away says the rest).
    static func decide(core: Core, id: String, approve: Bool, notify: Bool) {
        CoreCalls.run({ await core.agentRequestDecide(id: id, approve: approve) }) { a in
            if notify || !a.ok { Notifier.post(title: a.title, body: a.body) }
            answered()
        }
    }
}
