import Darwin
import Foundation

/// One request's connection, so it can be cancelled from another thread (a cancelled fetch): closing it makes the
/// app take the fetch back.
public final class FPConnection: @unchecked Sendable {
    private let lock = NSLock()
    private var fd: Int32
    private var cancelledFlag = false

    init(fd: Int32) { self.fd = fd }

    /// Ends the request; the waiting call returns `cancelled`.
    public func cancel() {
        lock.lock()
        defer { lock.unlock() }
        cancelledFlag = true
        if fd >= 0 { shutdown(fd, SHUT_RDWR) }
    }

    var cancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelledFlag
    }

    func close() {
        lock.lock()
        defer { lock.unlock() }
        if fd >= 0 { Darwin.close(fd) }
        fd = -1
    }
}

/// Starting the app when it is not running: at most once per `every`, then waiting up to `wait` for its socket.
public final class FPLaunchGate: @unchecked Sendable {
    private let lock = NSLock()
    private var last: Date?
    let every: TimeInterval
    let wait: TimeInterval
    private let launch: @Sendable () -> Void

    public init(every: TimeInterval = 30, wait: TimeInterval = 10, launch: @escaping @Sendable () -> Void) {
        (self.every, self.wait, self.launch) = (every, wait, launch)
    }

    /// Starts the app unless that was tried a moment ago. Returns how long to keep trying to connect.
    func startApp(now: Date = Date()) -> TimeInterval {
        lock.lock()
        let due = last.map { now.timeIntervalSince($0) >= every } ?? true
        if due { last = now }
        lock.unlock()
        guard due else { return 0 }
        launch()
        return wait
    }
}

/// The extension's side of the socket: one blocking call per request (call it off the system's queues).
public struct FPClient: Sendable {
    /// The group's container (where the socket is).
    public let dir: URL
    /// Starts the app when it is not running (`nil`: never; the tests' clients).
    public let gate: FPLaunchGate?
    /// Longest wait for any line of an answer (a fetch's progress comes four times a second).
    public var readTimeout: TimeInterval = 60

    public init(dir: URL, gate: FPLaunchGate?) {
        self.dir = dir
        self.gate = gate
    }

    /// Sends `request` and decodes the answer's `ok` as `T`. `progress` gets a fetch's progress lines, `opened` the
    /// connection (to cancel it).
    public func ask<T: Decodable>(_ request: FPRequest, as _: T.Type = T.self,
                                  progress: ((UInt64, UInt64) -> Void)? = nil,
                                  opened: ((FPConnection) -> Void)? = nil) -> Result<T, FPFailure> {
        let line: Data
        do { line = try request.line() } catch { return .failure(FPFailure(.badRequest, "\(error)")) }
        let fd: Int32
        switch connectOrStart() {
        case .success(let s): fd = s
        case .failure(let f): return .failure(f)
        }
        let conn = FPConnection(fd: fd)
        defer { conn.close() }
        opened?(conn)
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        var tv = timeval(tv_sec: Int(readTimeout), tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        let sent = line.withUnsafeBytes { p -> Bool in
            var off = 0
            while off < p.count {
                let n = write(fd, p.baseAddress! + off, p.count - off)
                if n <= 0 { return false }
                off += n
            }
            return true
        }
        guard sent else { return .failure(conn.cancelled ? FPFailure(.cancelled, "Cancelled.") : FPFailure(.notRunning, FPFailure.notRunningMessage)) }
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        let decoder = fpDecoder()
        while true {
            while let nl = buffer.firstIndex(of: 0x0A) {
                let l = buffer[buffer.startIndex..<nl]
                buffer = Data(buffer[(nl + 1)...])
                guard let parsed = try? decoder.decode(FPLine<T>.self, from: Data(l)) else {
                    return .failure(FPFailure(.badRequest, "Seedwell sent an answer this extension cannot read."))
                }
                if let p = parsed.progress {
                    progress?(p.done, p.total)
                    continue
                }
                if parsed.alive == true { continue }
                if let e = parsed.err {
                    return .failure(FPFailure(FPFailure.Kind(rawValue: e.kind) ?? .failed, e.message))
                }
                if let ok = parsed.ok { return .success(ok) }
                return .failure(FPFailure(.badRequest, "Seedwell sent an empty answer."))
            }
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 {
                if conn.cancelled { return .failure(FPFailure(.cancelled, "Cancelled.")) }
                // The app quit in the middle, or took too long to say anything.
                return .failure(FPFailure(.notRunning, FPFailure.notRunningMessage))
            }
            buffer.append(contentsOf: chunk[0..<n])
        }
    }

    /// Connects; when nothing listens, starts the app (through the gate) and keeps trying for a while.
    func connectOrStart() -> Result<Int32, FPFailure> {
        if let fd = try? connect() { return .success(fd) }
        let wait = gate?.startApp() ?? 0
        let end = Date().addingTimeInterval(wait)
        while Date() < end {
            usleep(250_000)
            if let fd = try? connect() { return .success(fd) }
        }
        return .failure(FPFailure(.notRunning, FPFailure.notRunningMessage))
    }

    /// A connected socket to `<dir>/fp`.
    func connect() throws -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw POSIXError(.EBADF) }
        let path = dir.appendingPathComponent(FPGroup.socketName).path
        let ok: Bool
        if path.utf8.count <= 103 {
            ok = Self.connect(fd, name: path)
        } else {
            // Longer than a socket path may be: by its name alone, from a thread whose own working folder is the
            // container (`pthread_fchdir_np`, as the app binds it; the process's working folder never changes).
            ok = Self.inFolder(dir.path) { Self.connect(fd, name: FPGroup.socketName) }
        }
        guard ok else {
            let e = errno
            Darwin.close(fd)
            throw POSIXError(POSIXErrorCode(rawValue: e) ?? .ECONNREFUSED)
        }
        return fd
    }

    private static func connect(_ fd: Int32, name: String) -> Bool {
        var a = sockaddr_un()
        a.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(name.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: a.sun_path) else { return false }
        withUnsafeMutableBytes(of: &a.sun_path) { p in
            for (i, b) in bytes.enumerated() { p[i] = b }
            p[bytes.count] = 0
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        return withUnsafePointer(to: &a) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, len) } } == 0
    }

    /// Runs `body` on a thread of its own whose working folder is `folder`.
    static func inFolder(_ folder: String, _ body: @escaping @Sendable () -> Bool) -> Bool {
        let dirfd = open(folder, O_RDONLY | O_DIRECTORY)
        guard dirfd >= 0 else { return false }
        defer { Darwin.close(dirfd) }
        let done = DispatchSemaphore(value: 0)
        let result = FPBox(false)
        let t = Thread {
            if let fchdir = threadChdir(), fchdir(dirfd) == 0 { result.value = body() }
            done.signal()
        }
        t.start()
        done.wait()
        return result.value
    }
}

/// A value handed back from another thread.
final class FPBox<T>: @unchecked Sendable {
    var value: T
    init(_ v: T) { value = v }
}

/// `pthread_fchdir_np`: sets the calling thread's own working folder (libsystem_pthread exports it, `fts` uses it;
/// the SDK has no header for it).
func threadChdir() -> (@convention(c) (Int32) -> Int32)? {
    guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "pthread_fchdir_np") else { return nil }  // RTLD_DEFAULT
    return unsafeBitCast(sym, to: (@convention(c) (Int32) -> Int32).self)
}
