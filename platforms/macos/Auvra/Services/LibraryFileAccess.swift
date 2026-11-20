import AppKit
import Darwin
import Foundation

struct LibraryConfiguration: Sendable { let directory: String; let roots: [AuthorizedRoot] }

enum BookmarkRecordFile {
    static let maximumBytes = 8 * 1024 * 1024

    static func read(_ url: URL) throws -> Data? {
        let fd = open(url.path, O_RDONLY | O_CLOEXEC | O_NONBLOCK | O_NOFOLLOW)
        guard fd >= 0 else {
            let code = errno
            if code == ENOENT { return nil }
            throw POSIXError(POSIXErrorCode(rawValue: code) ?? .EIO)
        }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        defer { try? handle.close() }
        var info = stat()
        guard fstat(fd, &info) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        guard info.st_mode & S_IFMT == S_IFREG, (0...Int64(maximumBytes)).contains(info.st_size) else {
            throw CocoaError(.fileReadCorruptFile)
        }
        var data = Data()
        // The file may grow after fstat; keep every read bounded as well.
        while let chunk = try handle.read(upToCount: min(64 * 1024, maximumBytes - data.count + 1)), !chunk.isEmpty {
            guard chunk.count <= maximumBytes - data.count else { throw CocoaError(.fileReadCorruptFile) }
            data.append(chunk)
        }
        return data
    }

    static func write(_ data: Data, to url: URL) throws {
        guard data.count <= maximumBytes else { throw CocoaError(.fileWriteOutOfSpace) }
        try data.write(to: url, options: .atomic)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
    }
}

/// Native selection belongs to MainActor. Bookmark and filesystem work belongs to the actor below.
@MainActor
enum LibraryFolderChooser {
    static func choose() async -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false; panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false; panel.canCreateDirectories = false
        panel.prompt = "Add Folder"; panel.message = "Choose a music folder to add to your library."
        return await panel.begin() == .OK ? panel.url : nil
    }
}

actor LibraryFileAccess {
    private struct Record: Codable, Sendable {
        let id: String
        var name: String
        var bookmark: Data
        var lastPath: String
    }
    private struct Scope { let id: String; let url: URL; let started: Bool }
    let directory: URL
    private var records: [Record] = []
    private var scopes: [String: Scope] = [:]
    // Removing a root affects the index/bookmark. A playing track can retain its session grant.
    private var retiredScopes: [Scope] = []
    private var loaded = false
    init(directory: URL? = nil) {
        self.directory = directory ?? URL.applicationSupportDirectory.appendingPathComponent("Auvra", isDirectory: true)
    }
    private var recordFile: URL { directory.appendingPathComponent("library-folders.json") }
    private func load() throws {
        guard !loaded else { return }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        if let data = try BookmarkRecordFile.read(recordFile) {
            let decoded = try JSONDecoder().decode([Record].self, from: data)
            guard decoded.count <= 64, Set(decoded.map(\.id)).count == decoded.count else {
                throw CocoaError(.fileReadCorruptFile)
            }
            records = decoded
        }
        loaded = true
    }
    private func persist(_ updated: [Record]) throws {
        try BookmarkRecordFile.write(JSONEncoder().encode(updated), to: recordFile)
        records = updated
    }
    private func acquire(_ url: URL, id: String) throws {
        if scopes[id]?.url.path == url.path { return }
        let started = url.startAccessingSecurityScopedResource()
        do {
            guard try url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory == true,
                  FileManager.default.isReadableFile(atPath: url.path) else { throw CocoaError(.fileReadNoPermission) }
        } catch { if started { url.stopAccessingSecurityScopedResource() }; throw error }
        if let old = scopes[id] { retiredScopes.append(old) }
        scopes[id] = Scope(id: id, url: url, started: started)
    }
    private func bookmark(_ url: URL) throws -> Data {
        try url.bookmarkData(options: [.withSecurityScope, .securityScopeAllowOnlyReadAccess],
                             includingResourceValuesForKeys: [.isDirectoryKey], relativeTo: nil)
    }
    func restore() throws -> LibraryConfiguration {
        try load()
        var updated = records
        var roots: [AuthorizedRoot] = []
        for index in updated.indices {
            let record = updated[index]
            do {
                var stale = false
                let url = try URL(resolvingBookmarkData: record.bookmark, options: [.withSecurityScope, .withoutUI],
                                  relativeTo: nil, bookmarkDataIsStale: &stale)
                try acquire(url, id: record.id)
                if stale { updated[index].bookmark = try bookmark(url) }
                let path = url.resolvingSymlinksInPath().standardizedFileURL.path
                updated[index].lastPath = path
                roots.append(AuthorizedRoot(id: record.id, name: record.name, path: path))
            } catch { roots.append(AuthorizedRoot(id: record.id, name: record.name, path: nil)) }
        }
        try persist(updated)
        return LibraryConfiguration(directory: directory.path, roots: roots)
    }
    func add(_ url: URL, replacing id: String? = nil) throws -> AuthorizedRoot {
        try load()
        let path = url.resolvingSymlinksInPath().standardizedFileURL.path
        let duplicate = records.first { $0.lastPath == path }
        let selectedId = id ?? duplicate?.id ?? UUID().uuidString
        if let duplicate, duplicate.id != selectedId { throw CocoaError(.fileWriteFileExists) }
        guard records.count < 64 || records.contains(where: { $0.id == selectedId }) else { throw CocoaError(.fileWriteOutOfSpace) }
        try acquire(url, id: selectedId)
        let record = Record(id: selectedId, name: url.lastPathComponent, bookmark: try bookmark(url), lastPath: path)
        var updated = records.filter { $0.id != selectedId }; updated.append(record)
        try persist(updated)
        return AuthorizedRoot(id: selectedId, name: record.name, path: path)
    }
    func remove(_ id: String) throws {
        try load(); try persist(records.filter { $0.id != id })
        if let scope = scopes.removeValue(forKey: id) { retiredScopes.append(scope) }
    }
    func sessionRoots() -> [SessionRootGrant] {
        var roots: [SessionRootGrant] = []
        for scope in Array(scopes.values) + retiredScopes {
            let path = scope.url.resolvingSymlinksInPath().standardizedFileURL.path
            if !roots.contains(where: { $0.id == scope.id }) { roots.append(SessionRootGrant(id: scope.id, path: path)) }
        }
        return Array(roots.prefix(64))
    }
    deinit {
        for scope in Array(scopes.values) + retiredScopes where scope.started { scope.url.stopAccessingSecurityScopedResource() }
    }
}
