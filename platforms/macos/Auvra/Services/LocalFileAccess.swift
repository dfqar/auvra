import AppKit
import Foundation
import UniformTypeIdentifiers

/// The host retains access grants; Rust receives only an authorized filesystem path.
@MainActor
final class LocalFileAccess {
    private var scopedURLs: [String: URL] = [:]
    func chooseAudioFile() async -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.audio]
        panel.message = "Choose a local audio file."
        panel.prompt = "Open"
        return await panel.begin() == .OK ? panel.url : nil
    }
    func chooseSoloistExecutable() async -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.resolvesAliases = false
        panel.directoryURL = .downloadsDirectory
        panel.message = "Choose the extracted soloist executable from the official Apple Silicon download."
        panel.prompt = "Install"
        return await panel.begin() == .OK ? panel.url : nil
    }
    func retainAccess(to url: URL) {
        guard scopedURLs[url.path] == nil else { return }
        if url.startAccessingSecurityScopedResource() { scopedURLs[url.path] = url }
    }
    deinit { for url in scopedURLs.values { url.stopAccessingSecurityScopedResource() } }
}

/// Persistent file grants for queue restoration; all bookmark IO stays off MainActor.
actor SessionFileBookmarks {
    private struct Record: Codable { let originalPath: String; var lastPath: String; var bookmark: Data }
    private struct Scope { let url: URL; let started: Bool }
    private let directory: URL
    private var records: [Record] = []
    private var scopes: [Scope] = []
    private var loaded = false
    init(directory: URL? = nil) { self.directory = directory ?? URL.applicationSupportDirectory.appendingPathComponent("Auvra", isDirectory: true) }
    private var recordFile: URL { directory.appendingPathComponent("queue-files.json") }
    private func load() throws {
        guard !loaded else { return }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        if let data = try BookmarkRecordFile.read(recordFile) {
            let decoded = try JSONDecoder().decode([Record].self, from: data)
            guard decoded.count <= 64 else { throw CocoaError(.fileReadCorruptFile) }
            records = decoded
        }
        loaded = true
    }
    private func bookmark(_ url: URL) throws -> Data {
        try url.bookmarkData(options: [.withSecurityScope, .securityScopeAllowOnlyReadAccess], includingResourceValuesForKeys: [.isRegularFileKey], relativeTo: nil)
    }
    private func acquire(_ url: URL) throws {
        if scopes.contains(where: { $0.url.path == url.path }) { return }
        let started = url.startAccessingSecurityScopedResource()
        guard (try? url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true,
              FileManager.default.isReadableFile(atPath: url.path) else {
            if started { url.stopAccessingSecurityScopedResource() }; throw CocoaError(.fileReadNoPermission)
        }
        scopes.append(Scope(url: url, started: started))
    }
    private func persist(_ updated: [Record]) throws {
        try BookmarkRecordFile.write(JSONEncoder().encode(updated), to: recordFile)
        records = updated
    }
    func remember(_ url: URL) throws {
        try load(); try acquire(url)
        let path = url.resolvingSymlinksInPath().standardizedFileURL.path
        let original = records.first(where: { $0.lastPath == path })?.originalPath ?? path
        var updated = records.filter { $0.originalPath != original }
        updated.append(Record(originalPath: original, lastPath: path, bookmark: try bookmark(url)))
        if updated.count > 64 { updated.removeFirst(updated.count - 64) }
        try persist(updated)
    }
    func restore() throws -> [SessionFileGrant] {
        try load(); var updated = records; var grants: [SessionFileGrant] = []
        for index in updated.indices {
            do {
                var stale = false
                let url = try URL(resolvingBookmarkData: updated[index].bookmark, options: [.withSecurityScope, .withoutUI], relativeTo: nil, bookmarkDataIsStale: &stale)
                try acquire(url)
                if stale { updated[index].bookmark = try bookmark(url) }
                updated[index].lastPath = url.resolvingSymlinksInPath().standardizedFileURL.path
                grants.append(SessionFileGrant(originalPath: updated[index].originalPath, path: updated[index].lastPath))
            } catch { grants.append(SessionFileGrant(originalPath: updated[index].originalPath, path: nil)) }
        }
        try persist(updated); return grants
    }
    deinit { for scope in scopes where scope.started { scope.url.stopAccessingSecurityScopedResource() } }
}

/// Bounded file IO for an explicitly chosen playlist cover; no work on the playback path.
@MainActor
extension PlayerStore {
    func chooseSpotifyPlaylistCover(_ id: String) async {
        guard playlistEditAccess(id: id).canEditDetails, !spotifyEditPending else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.jpeg]
        panel.message = "Choose a JPEG to upload as this Spotify playlist’s cover (up to 192 KiB)."
        guard await panel.begin() == .OK, let url = panel.url else { return }
        let encoded = await Task.detached(priority: .utility) { () -> String? in
            guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
            defer { try? handle.close() }
            guard let data = try? handle.read(upToCount: 196609), !data.isEmpty, data.count <= 196608,
                  data.starts(with: [0xff, 0xd8, 0xff]) else { return nil }
            return data.base64EncodedString()
        }.value
        guard let encoded else { errorMessage = "Choose a JPEG cover no larger than 192 KiB."; return }
        editSpotify(.init(kind: "playlist_cover", jpegBase64: encoded, id: id))
    }
}
