import Foundation

enum LibraryMode: String, Codable, Sendable, CaseIterable { case tracks, albums }
struct AuthorizedRoot: Codable, Sendable { let id, name: String; let path: String? }
struct LibraryQuery: Codable, Sendable {
    let queryId: UInt64
    var mode: LibraryMode = .albums
    var text = ""
    var albumId: String?
    var rootId: String?
    var offset: UInt32 = 0
    var limit: UInt32 = 100
}
struct LibraryTrack: Decodable, Sendable, Identifiable {
    let item: MediaItem
    let albumId: String
    let trackNumber, discNumber: UInt32?
    let available: Bool
    let issue: String?
    var id: String { item.id }
}
struct LibraryAlbum: Decodable, Sendable, Identifiable {
    let id, title, artist: String
    let year: UInt32?
    let artwork: String?
    let trackCount, availableCount, durationMs: UInt64
}
struct LibraryRoot: Decodable, Sendable, Identifiable {
    let id, name, path: String
    let authorized: Bool
    let status: String
    let issue: String?
    let trackCount: UInt64
    let lastScanMs: UInt64?
}
struct ScanProgress: Decodable, Sendable {
    let rootId: String
    let visited, imported, unchanged, failed: UInt64
}
struct LibraryState: Decodable, Sendable {
    let revision: UInt64
    let ready: Bool
    let roots: [LibraryRoot]
    let totalTracks, totalAlbums: UInt64
    let scan, lastScan: ScanProgress?
    let issue: String?
}
struct LibraryPage: Decodable, Sendable {
    let queryId, revision: UInt64
    let offset: UInt32
    let total: UInt64
    let tracks: [LibraryTrack]
    let albums: [LibraryAlbum]
}
