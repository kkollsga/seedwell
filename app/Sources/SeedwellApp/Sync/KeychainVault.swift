import Foundation
import Security

/// Folder sync's keys in the Keychain (SPEC 7G.1): generic passwords, service `io.github.kkollsga.Seedwell.sync`,
/// account = the secret's name ("device", "folder-<id>"), readable after the first unlock and on this device only
/// (never in a backup or iCloud Keychain). Called by the core from its own threads.
///
/// The data protection keychain is used when the build may (a signed app with a keychain entitlement); an
/// ad-hoc or Developer ID build without one gets `errSecMissingEntitlement`, and the login keychain is used
/// instead, where the item's access list names this app. The choice is made once per run.
final class KeychainVault: SecretVault, @unchecked Sendable {
    static let service = "io.github.kkollsga.Seedwell.sync"
    private let service: String
    private let lock = NSLock()
    private var dataProtection: Bool?

    init(service: String = KeychainVault.service) { self.service = service }

    private func base(_ name: String, dp: Bool) -> [String: Any] {
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: name,
        ]
        if dp { q[kSecUseDataProtectionKeychain as String] = true }
        return q
    }

    /// Whether the data protection keychain works for this build (asked once).
    private func usesDataProtection() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if let d = dataProtection { return d }
        var q = base("probe", dp: true)
        q[kSecReturnData as String] = false
        let status = SecItemCopyMatching(q as CFDictionary, nil)
        let ok = status != errSecMissingEntitlement
        dataProtection = ok
        return ok
    }

    private func fail(_ what: String, _ status: OSStatus) -> VaultError {
        let text = SecCopyErrorMessageString(status, nil) as String? ?? "error \(status)"
        return .Failed(message: "\(what): \(text)")
    }

    func get(name: String) throws -> Data? {
        var q = base(name, dp: usesDataProtection())
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
        let dp = usesDataProtection()
        let q = base(name, dp: dp)
        var attrs: [String: Any] = [kSecValueData as String: value]
        if dp { attrs[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly }
        var status = SecItemUpdate(q as CFDictionary, attrs as CFDictionary)
        if status == errSecItemNotFound {
            var add = q
            add.merge(attrs) { $1 }
            add[kSecAttrLabel as String] = "Seedwell folder sync (\(name.hasPrefix("folder-") ? "folder key" : "device key"))"
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            status = SecItemAdd(add as CFDictionary, nil)
            // The login keychain may refuse the accessibility class: its items are this Mac's anyway.
            if status == errSecParam, !dp {
                add[kSecAttrAccessible as String] = nil
                status = SecItemAdd(add as CFDictionary, nil)
            }
        }
        guard status == errSecSuccess else { throw fail("Couldn't save “\(name)” in the Keychain", status) }
    }

    func delete(name: String) throws {
        let status = SecItemDelete(base(name, dp: usesDataProtection()) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw fail("Couldn't remove “\(name)” from the Keychain", status) }
    }
}
