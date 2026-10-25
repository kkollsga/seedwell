import Foundation
import Security

/// Folder sync's keys in the Keychain (SPEC 7G.1): generic passwords in the data protection keychain, service
/// `io.github.kkollsga.Seedwell.sync`, account = the secret's name ("device", "folder-<id>"), readable after the
/// first unlock and on this device only (never in a backup or iCloud Keychain). Called by the core from its own
/// threads.
///
/// Only a build with a keychain entitlement may use it ([`available`]). An ad-hoc build (the curl-installed
/// releases) may not, and the login keychain is no way out: its access list names the exact build, so macOS
/// would ask for the user's password after every update. Those builds keep the keys in a 0600 file in the app's
/// state folder instead (the core's own store).
final class KeychainVault: SecretVault, @unchecked Sendable {
    static let service = "io.github.kkollsga.Seedwell.sync"
    private let service: String

    init(service: String = KeychainVault.service) { self.service = service }

    private static func base(_ service: String, _ name: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: name,
            kSecUseDataProtectionKeychain as String: true,
        ]
    }

    /// Whether this build may write to the data protection keychain. A read does not tell (without the
    /// entitlement it answers "not found"), so a probe item is written and removed again.
    static func available(service: String = KeychainVault.service) -> Bool {
        let q = base(service, "probe")
        var add = q
        add[kSecValueData as String] = Data([1])
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(add as CFDictionary, nil)
        SecItemDelete(q as CFDictionary)
        return status == errSecSuccess || status == errSecDuplicateItem
    }

    private func fail(_ what: String, _ status: OSStatus) -> VaultError {
        let text = SecCopyErrorMessageString(status, nil) as String? ?? "error \(status)"
        return .Failed(message: "\(what): \(text)")
    }

    func get(name: String) throws -> Data? {
        var q = Self.base(service, name)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &out)
        switch status {
        case errSecSuccess: return out as? Data
        case errSecItemNotFound: return nil
        default: throw fail("Couldn't read “\(name)” from the Keychain", status)
        }
    }

    func put(name: String, value: Data) throws {
        let q = Self.base(service, name)
        let attrs: [String: Any] = [
            kSecValueData as String: value,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        var status = SecItemUpdate(q as CFDictionary, attrs as CFDictionary)
        if status == errSecItemNotFound {
            var add = q
            add.merge(attrs) { $1 }
            add[kSecAttrLabel as String] = "Seedwell folder sync (\(name.hasPrefix("folder-") ? "folder key" : "device key"))"
            status = SecItemAdd(add as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw fail("Couldn't save “\(name)” in the Keychain", status) }
    }

    func delete(name: String) throws {
        let status = SecItemDelete(Self.base(service, name) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw fail("Couldn't remove “\(name)” from the Keychain", status) }
    }
}
