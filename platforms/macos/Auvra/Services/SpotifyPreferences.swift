import Foundation

/// Small host preferences only: never credentials, temporary image URLs or remote queues.
@MainActor
final class SpotifyPreferences {
    enum AccountKey: Equatable {
        case profile(String)
        case stable(String)
    }
    struct Account {
        let profileID: String
        let stableID: String?

        init(profileID: String, stableID: String? = nil) {
            self.profileID = profileID
            self.stableID = stableID
        }

        var key: AccountKey { stableID.map(AccountKey.stable) ?? .profile(profileID) }
        fileprivate var isValid: Bool {
            SpotifyPreferences.validAccountID(profileID, stable: false)
                && (stableID.map { SpotifyPreferences.validAccountID($0, stable: true) } ?? true)
        }
    }
    struct Pin: Codable, Equatable {
        let accountID, playlistID, title: String
        fileprivate var stableAccount: Bool? = nil
        fileprivate var key: AccountKey { stableAccount == true ? .stable(accountID) : .profile(accountID) }
        fileprivate var isValid: Bool {
            SpotifyPreferences.validAccountID(accountID, stable: stableAccount == true) &&
            playlistID.utf8.count == 22 && playlistID.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) &&
            title.utf8.count <= 1024
        }
    }
    private struct Record: Codable { let version: Int; let pins: [Pin] }
    // Keep the original key so both namespaces are bounded and rewritten together.
    static let key = "AuvraSpotifyFavoritesV1"
    private let defaults: UserDefaults
    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    nonisolated static func validAccountID(_ id: String, stable: Bool) -> Bool {
        !id.isEmpty && id.utf8.count <= 256 && !id.unicodeScalars.contains { scalar in
            CharacterSet.controlCharacters.contains(scalar) || (stable && CharacterSet.whitespacesAndNewlines.contains(scalar))
        }
    }

    private var pins: [Pin] {
        guard let data = defaults.data(forKey: Self.key), data.count <= 16 * 1024,
              let record = try? JSONDecoder().decode(Record.self, from: data), (1...2).contains(record.version),
              record.pins.count <= 8 else { return [] }
        return record.pins.filter { $0.isValid && (record.version == 2 || $0.stableAccount == nil) }
    }

    func favorite(for account: Account) -> Pin? {
        guard account.isValid else { return nil }
        let current = pins
        let favorite = current.last { $0.key == account.key }
        let legacyKey = AccountKey.profile(account.profileID)
        guard let stableID = account.stableID, let legacy = current.last(where: { $0.key == legacyKey }) else { return favorite }
        var updated = current.filter { $0.key != legacyKey }
        if let favorite {
            _ = save(updated)
            return favorite
        }
        let promoted = Pin(accountID: stableID, playlistID: legacy.playlistID, title: legacy.title, stableAccount: true)
        updated.append(promoted)
        return save(updated) ? promoted : nil
    }

    @discardableResult
    func setFavorite(_ playlist: RemoteCollection?, for account: Account) -> Bool {
        guard account.isValid else { return false }
        let legacyKey = AccountKey.profile(account.profileID)
        var updated = pins.filter { $0.key != account.key && (account.stableID == nil || $0.key != legacyKey) }
        if let playlist {
            let pin = Pin(accountID: account.stableID ?? account.profileID, playlistID: playlist.id,
                          title: String(playlist.title.prefix(160)), stableAccount: account.stableID.map { _ in true })
            guard playlist.entity == .playlist, pin.isValid else { return false }
            updated.append(pin)
        }
        return save(updated)
    }

    private func save(_ pins: [Pin]) -> Bool {
        let record = Record(version: 2, pins: Array(pins.suffix(8)))
        let encoder = JSONEncoder()
        encoder.outputFormatting = .withoutEscapingSlashes
        guard let data = try? encoder.encode(record), data.count <= 16 * 1024 else { return false }
        defaults.set(data, forKey: Self.key)
        // The old unbound ID cannot safely be assigned to whichever account signs in next.
        defaults.removeObject(forKey: "AuvraFavoritePlaylistID")
        return true
    }
}
