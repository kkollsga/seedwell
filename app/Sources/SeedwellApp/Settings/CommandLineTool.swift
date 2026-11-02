import Foundation

/// The `seedwell` command inside the app bundle: where it is, linking it onto the user's PATH and installing the
/// Claude Code skill (SPEC 9A). Whether it and the skill are installed is the core's to find (`refreshSettingsHostFacts`). Nothing here runs unless the user presses a button
/// in Settings > Agents.
enum CommandLineTool {
    /// The CLI inside the bundle (next to the app executable).
    static func bundledPath(bundlePath: String = Bundle.main.bundlePath) -> String {
        bundlePath.hasSuffix(".app")
            ? bundlePath + "/Contents/MacOS/seedwell"
            : "/Applications/\(AppInfo.name).app/Contents/MacOS/seedwell"
    }

    /// True when `~/.local/bin` (or another folder) is on the PATH, so a link there works at once.
    static func isOnPath(_ dir: String, pathEnv: String) -> Bool {
        pathEnv.split(separator: ":").map(String.init).contains(dir)
    }

    enum LinkError: Error, Equatable {
        case exists(String)
        case failed(String)
    }

    /// Link `<dir>/seedwell` to `target`. Creates `dir` (only the user's own `~/.local/bin` is ever created).
    /// An existing link to something else is left alone and reported.
    @discardableResult
    static func link(into dir: String, target: String, fm: FileManager = .default) throws -> String {
        let path = dir + "/seedwell"
        do {
            if !fm.fileExists(atPath: dir) { try fm.createDirectory(atPath: dir, withIntermediateDirectories: true) }
            if let dest = try? fm.destinationOfSymbolicLink(atPath: path) {
                if dest == target { return path }
                throw LinkError.exists(path)
            }
            if fm.fileExists(atPath: path) { throw LinkError.exists(path) }
            try fm.createSymbolicLink(atPath: path, withDestinationPath: target)
            return path
        } catch let e as LinkError {
            throw e
        } catch {
            throw LinkError.failed(error.localizedDescription)
        }
    }

    struct Result: Equatable { var status: Int32; var out: String; var err: String }

    /// Run the bundled CLI (never against the app's control socket: the skill commands do not need it).
    static func run(_ args: [String], cli: String = bundledPath()) async -> Result {
        await Task.detached {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: cli)
            p.arguments = args
            let out = Pipe(), err = Pipe()
            p.standardOutput = out
            p.standardError = err
            do { try p.run() } catch { return Result(status: 127, out: "", err: error.localizedDescription) }
            let o = out.fileHandleForReading.readDataToEndOfFile()
            let e = err.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            return Result(status: p.terminationStatus, out: String(decoding: o, as: UTF8.self), err: String(decoding: e, as: UTF8.self))
        }.value
    }
}
