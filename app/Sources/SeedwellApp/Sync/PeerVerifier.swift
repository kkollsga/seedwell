import Foundation
import Security

/// Who may use the File Provider socket (SPEC 7G.11): only this app's own extension. The core's server hands over the
/// peer's audit token before it reads anything of a request; this checks the peer's code signature against a
/// requirement: the extension's identifier signed by the team's Developer ID when this app is, else (an ad-hoc or
/// development build) exactly the extension inside this app's bundle, by its code directory hash.
final class PeerVerifier: Sendable {
    static let extensionID = "io.github.kkollsga.Seedwell.FileProvider"
    static let team = "6GND8Z2C7Z"

    /// The requirement's text, `nil` when there is none (no team signature and no extension to pin): every peer is
    /// refused then.
    let text: String?
    private let requirement: SecRequirementBox?

    convenience init(appBundle: URL = Bundle.main.bundleURL) {
        let appex = appBundle.appendingPathComponent("Contents/PlugIns/SeedwellFileProvider.appex")
        self.init(text: Self.requirementText(teamSigned: Self.teamOfThisApp() == Self.team, cdhash: Self.cdhash(of: appex)))
    }

    /// A verifier for this requirement (tests check one against a process that does not satisfy it).
    init(text: String?) {
        self.text = text
        requirement = text.flatMap { t in
            var r: SecRequirement?
            return SecRequirementCreateWithString(t as CFString, [], &r) == errSecSuccess ? r.map(SecRequirementBox.init) : nil
        }
    }

    /// The team's extension when this app is the team's; else the very extension shipped inside it.
    static func requirementText(teamSigned: Bool, cdhash: Data?) -> String? {
        if teamSigned {
            return "identifier \"\(extensionID)\" and anchor apple generic and certificate leaf[subject.OU] = \"\(team)\""
        }
        guard let cdhash, !cdhash.isEmpty else { return nil }
        return "identifier \"\(extensionID)\" and cdhash H\"\(cdhash.map { String(format: "%02x", $0) }.joined())\""
    }

    /// There is a requirement to check against (else every peer is refused).
    var hasRequirement: Bool { requirement != nil }

    /// The process with this audit token is this app's extension.
    func verify(_ auditToken: Data) -> Bool {
        guard auditToken.count == MemoryLayout<audit_token_t>.size, let requirement else { return false }
        var code: SecCode?
        let attrs = [kSecGuestAttributeAudit: auditToken] as CFDictionary
        guard SecCodeCopyGuestWithAttributes(nil, attrs, [], &code) == errSecSuccess, let code else { return false }
        return SecCodeCheckValidity(code, [], requirement.value) == errSecSuccess
    }

    /// The team that signed this app (none when ad hoc).
    static func teamOfThisApp() -> String? {
        var me: SecCode?
        var stat: SecStaticCode?
        guard SecCodeCopySelf([], &me) == errSecSuccess, let me, SecCodeCopyStaticCode(me, [], &stat) == errSecSuccess, let stat else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(stat, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess else { return nil }
        return (info as? [String: Any])?[kSecCodeInfoTeamIdentifier as String] as? String
    }

    /// The code directory hash of the code at `url`.
    static func cdhash(of url: URL) -> Data? {
        var stat: SecStaticCode?
        guard SecStaticCodeCreateWithPath(url as CFURL, [], &stat) == errSecSuccess, let stat else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(stat, [], &info) == errSecSuccess else { return nil }
        return (info as? [String: Any])?[kSecCodeInfoUnique as String] as? Data
    }
}

/// A requirement, handed between threads (it is immutable).
final class SecRequirementBox: @unchecked Sendable {
    let value: SecRequirement
    init(_ v: SecRequirement) { value = v }
}
