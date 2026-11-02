import Foundation

/// The core's slow calls are `async` (the FFI's `async_calls`): each returns at once and does its work on the core's own
/// threads, so the main thread never waits for the engine, a disk or the network. This is how the app starts them.
@MainActor enum CoreCalls {
    /// The user's commands to the core, in the order they were given: each starts once the one before it ended (a Pause
    /// and the Resume right after it reach the engine in that order).
    private static var tail: Task<Void, Never>?

    /// Runs `work` after every command given before it, then `then` on the main thread with its result (a refresh, an
    /// alert).
    static func inOrder<T: Sendable>(_ work: @escaping @Sendable () async -> T, then: @escaping @MainActor (T) -> Void) {
        let previous = tail
        tail = Task { @MainActor in
            await previous?.value
            then(await work())
        }
    }

    /// Runs `work` after every command given before it.
    static func inOrder(_ work: @escaping @Sendable () async -> Void) {
        inOrder(work, then: { (_: Void) in })
    }

    /// Runs `work` (a read, or a command whose order does not matter), then hands its result to `then` on the main thread.
    static func run<T: Sendable>(_ work: @escaping @Sendable () async -> T, then: @escaping @MainActor (T) -> Void) {
        Task { @MainActor in then(await work()) }
    }

    /// QA and tests: waits until every command given so far has run.
    static func settled() async {
        await tail?.value
    }
}

/// The result of a throwing `async` call, for an alert or a sheet's error line.
func catching<T: Sendable>(_ work: @Sendable () async throws -> T) async -> Result<T, Error> {
    do { return .success(try await work()) } catch { return .failure(error) }
}
