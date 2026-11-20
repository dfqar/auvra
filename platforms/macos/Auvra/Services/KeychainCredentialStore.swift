import Foundation
import Security

struct CredentialFailure: LocalizedError {
    let status: OSStatus
    var errorDescription: String? { "Secure login storage is unavailable (\(status))." }
}
/// Security.framework calls are serialized off MainActor.
actor KeychainCredentialStore: CredentialStore {
    private let service: String
    init(service: String = "app.auvra.credentials") { self.service = service }
    private func query(_ account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: account]
    }
    func read(account: String) throws -> Data? {
        var query = query(account); query[kSecReturnData as String] = true; query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else { throw CredentialFailure(status: status) }
        return data
    }
    func write(_ value: Data, account: String) throws {
        let query = query(account)
        let status = SecItemUpdate(query as CFDictionary, [kSecValueData as String: value] as CFDictionary)
        if status == errSecSuccess { return }
        guard status == errSecItemNotFound else { throw CredentialFailure(status: status) }
        var item = query; item[kSecValueData as String] = value
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let added = SecItemAdd(item as CFDictionary, nil)
        guard added == errSecSuccess else { throw CredentialFailure(status: added) }
    }
    func remove(account: String) throws {
        let status = SecItemDelete(query(account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw CredentialFailure(status: status) }
    }
}
