import Foundation
import Security

final class AokieKeychainStore {
    private let service: String
    private let lock = NSLock()

    init(service: String = "com.aokie.companion.native.v1") {
        self.service = service
    }

    func put(account: String, data: Data) throws {
        lock.lock()
        defer { lock.unlock() }
        var query = baseQuery(account: account)
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        let update = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        if update == errSecSuccess { return }
        guard update == errSecItemNotFound else {
            throw AokieNativeRuntimeError.secureStore(update)
        }
        query.merge(attributes) { _, new in new }
        let add = SecItemAdd(query as CFDictionary, nil)
        guard add == errSecSuccess else {
            throw AokieNativeRuntimeError.secureStore(add)
        }
    }

    func get(account: String) throws -> Data? {
        lock.lock()
        defer { lock.unlock() }
        var query = baseQuery(account: account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw AokieNativeRuntimeError.secureStore(status)
        }
        return data
    }

    func delete(account: String) throws {
        lock.lock()
        defer { lock.unlock() }
        let status = SecItemDelete(baseQuery(account: account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw AokieNativeRuntimeError.secureStore(status)
        }
    }

    private func baseQuery(account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrSynchronizable as String: false,
        ]
    }
}

public final class AokieCredentialVault {
    private static let managedAccount = "managed-credential-envelope-v1"
    private let store: AokieKeychainStore

    init(store: AokieKeychainStore) {
        self.store = store
    }

    public convenience init() {
        self.init(store: AokieKeychainStore())
    }

    public func save(_ envelope: AokieManagedCredentialEnvelope) throws {
        let validated = try envelope.validated()
        let data = try JSONEncoder().encode(validated)
        guard data.count <= 32 * 1024 else { throw AokieNativeRuntimeError.encoding }
        try store.put(account: Self.managedAccount, data: data)
    }

    public func restore() throws -> AokieManagedCredentialEnvelope? {
        guard let data = try store.get(account: Self.managedAccount), data.count <= 32 * 1024 else {
            return nil
        }
        return try JSONDecoder().decode(AokieManagedCredentialEnvelope.self, from: data).validated()
    }

    public func forget() throws {
        try store.delete(account: Self.managedAccount)
    }
}
