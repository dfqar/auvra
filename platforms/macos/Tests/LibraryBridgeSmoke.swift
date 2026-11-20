import Foundation

struct LibrarySmokeFailure: Error { let message: String }
@main
struct LibraryBridgeSmoke {
    static func checkBookmarkBounds(in directory: URL) async throws {
        let folder = directory.appendingPathComponent("Music", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let file = folder.appendingPathComponent("Original.wav")
        try Data().write(to: file)
        let root = try await LibraryFileAccess(directory: directory).add(folder)
        try await SessionFileBookmarks(directory: directory).remember(file)

        let folders = LibraryFileAccess(directory: directory)
        let queue = SessionFileBookmarks(directory: directory)
        for name in ["library-folders.json", "queue-files.json"] {
            let url = directory.appendingPathComponent(name)
            let original = try Data(contentsOf: url)
            let handle = try FileHandle(forWritingTo: url)
            let oversized = UInt64(BookmarkRecordFile.maximumBytes) + 1
            try handle.truncate(atOffset: oversized)
            try handle.close()
            do {
                if name == "library-folders.json" { _ = try await folders.restore() }
                else { _ = try await queue.restore() }
                throw LibrarySmokeFailure(message: "Oversized bookmark record accepted")
            } catch let error as CocoaError where error.code == .fileReadCorruptFile {}
            let check = try FileHandle(forReadingFrom: url)
            let prefix = try check.read(upToCount: original.count)
            let size = try check.seekToEnd()
            try check.close()
            guard size == oversized, prefix == original else {
                throw LibrarySmokeFailure(message: "Failed restore replaced the saved bookmarks")
            }
            // Old-format JSON remains valid right up to the byte limit.
            var padded = original
            padded.append(Data(repeating: 0x20, count: BookmarkRecordFile.maximumBytes - original.count))
            try padded.write(to: url, options: .atomic)
        }
        let restoredFolders = try await folders.restore()
        let restoredQueue = try await queue.restore()
        guard restoredFolders.roots.first?.id == root.id,
              restoredFolders.roots.first?.path == folder.resolvingSymlinksInPath().standardizedFileURL.path,
              restoredQueue.first?.path == file.resolvingSymlinksInPath().standardizedFileURL.path else {
            throw LibrarySmokeFailure(message: "Original folder/file bookmark could not be restored after repair")
        }
        let queueURL = directory.appendingPathComponent("queue-files.json")
        let saved = try Data(contentsOf: queueURL)
        do {
            try BookmarkRecordFile.write(Data(count: BookmarkRecordFile.maximumBytes + 1), to: queueURL)
            throw LibrarySmokeFailure(message: "Oversized save replaced valid bookmarks")
        } catch let error as CocoaError where error.code == .fileWriteOutOfSpace {}
        guard try Data(contentsOf: queueURL) == saved else {
            throw LibrarySmokeFailure(message: "Failed save modified the bookmark record")
        }
        print("Folder and file bookmark byte limits, unchanged records on error, and retry of existing bookmarks passed")
    }

    @MainActor
    static func wait(_ label: String, player: PlayerStore, condition: @MainActor () -> Bool) async throws {
        for _ in 0..<400 {
            if let error = player.errorMessage { throw LibrarySmokeFailure(message: "\(label): \(error)") }
            if condition() { return }
            try await Task.sleep(for: .milliseconds(25))
        }
        throw LibrarySmokeFailure(message: "Timed out: \(label)")
    }
    @MainActor
    static func main() async throws {
        guard CommandLine.arguments.count >= 4 else { throw LibrarySmokeFailure(message: "phase storage fixtures") }
        let phase = CommandLine.arguments[1]
        let directory = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        let fixtures = URL(fileURLWithPath: CommandLine.arguments[3], isDirectory: true)
        if phase == "import" || phase == "bookmarks" {
            try await checkBookmarkBounds(in: directory.appendingPathComponent("bookmark-size-check", isDirectory: true))
            if phase == "bookmarks" { return }
        }
        if phase == "lost-access" {
            let recordFile = directory.appendingPathComponent("library-folders.json")
            var records = try JSONSerialization.jsonObject(with: Data(contentsOf: recordFile)) as! [[String: Any]]
            records[0]["bookmark"] = Data("invalid bookmark".utf8).base64EncodedString()
            try JSONSerialization.data(withJSONObject: records).write(to: recordFile, options: .atomic)
        }
        if phase == "import" {
            let bookmarkDirectory = directory.appendingPathComponent("bookmark-move-check", isDirectory: true)
            let original = directory.appendingPathComponent("Original Folder", isDirectory: true)
            let moved = directory.appendingPathComponent("Moved Folder", isDirectory: true)
            try FileManager.default.createDirectory(at: original, withIntermediateDirectories: true)
            let access = LibraryFileAccess(directory: bookmarkDirectory)
            let root = try await access.add(original)
            try FileManager.default.moveItem(at: original, to: moved)
            let restored = try await access.restore()
            guard restored.roots.first?.id == root.id, restored.roots.first?.path == moved.path else {
                throw LibrarySmokeFailure(message: "Moved folder bookmark was not resolved/refreshed")
            }
        }
        let player = PlayerStore(libraryDirectory: directory, loadSession: false)
        try await wait("library initialized", player: player) { player.libraryState?.ready == true }
        if phase == "pages" {
            let root = directory.appendingPathComponent("Paged Music", isDirectory: true)
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            let signal = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[4]))
            for index in 0..<240 { try signal.write(to: root.appendingPathComponent("track-\(index).flac")) }
            await player.addLibraryFolder(root)
            try await wait("paged scan", player: player) { player.libraryState?.scan == nil && player.libraryState?.totalTracks == 240 }
            player.browseLibrary(mode: .tracks, text: "")
            try await wait("first page", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.tracks.count == 100 && player.libraryPage?.offset == 0 }
            let first = player.libraryPage?.tracks.first?.id
            player.libraryPageBy(1)
            try await wait("second page", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.offset == 100 && player.libraryPage?.tracks.count == 100 }
            guard player.libraryPage?.tracks.first?.id != first else { throw LibrarySmokeFailure(message: "Page did not advance") }
            player.libraryPageBy(1)
            try await wait("last page", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.offset == 200 && player.libraryPage?.tracks.count == 40 }
            player.libraryPageBy(-1)
            try await wait("previous page", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.offset == 100 }
            player.browseLibrary(mode: .tracks, text: "no such track")
            player.browseLibrary(mode: .tracks, text: "FIRST LIGHT")
            try await wait("superseded query", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.total == 240 && player.libraryPage?.offset == 0 }
            print("M3 pages: 100/100/40 results, previous/next and superseded Unicode-normalized query passed")
            return
        }
        if phase == "import" {
            await player.addLibraryFolder(fixtures)
        } else if phase == "lost-access" {
            try await wait("missing grant", player: player) { player.libraryState?.roots.first?.authorized == false && player.libraryPage != nil }
            player.browseLibrary(mode: .tracks)
            try await wait("unavailable tracks", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.total == 3 && player.libraryPage?.tracks.allSatisfy({ !$0.available && !$0.item.capabilities.play }) == true }
            guard let id = player.libraryState?.roots.first?.id else { throw LibrarySmokeFailure(message: "Lost persisted root") }
            await player.addLibraryFolder(fixtures, replacing: id)
        }
        try await wait("complete catalog", player: player) {
            player.libraryState?.roots.count == 1 && player.libraryState?.totalTracks == 3 && player.libraryState?.scan == nil && player.libraryState?.lastScan != nil
        }
        if phase != "import", player.libraryState?.lastScan?.unchanged != 3 {
            throw LibrarySmokeFailure(message: "Restart/reconnect re-parsed unchanged metadata")
        }
        player.browseLibrary(mode: .albums, text: "")
        try await wait("album page", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.albums.count == 2 }
        guard let album = player.libraryPage?.albums.first(where: { $0.title == "Quiet Hours" }),
              album.trackCount == 2, album.artist == "Auvra Sessions", let artwork = album.artwork,
              let image = await LocalArtworkLoader.shared.image(for: artwork), image.size.width > 0 else {
            throw LibrarySmokeFailure(message: "Album grouping/artwork thumbnail failed")
        }
        if phase == "import" {
            player.openLibraryAlbum(album)
            try await wait("album details", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.tracks.count == 2 && player.libraryQuery.albumId == album.id }
            guard player.libraryPage?.tracks.first?.item.title == "Тихий берег" else { throw LibrarySmokeFailure(message: "Track number ordering failed") }
            player.setVolume(0.3); player.playLibraryAlbum()
            try await wait("library audio", player: player) { player.isPlaying && player.current?.title == "Тихий берег" && player.current?.durationMs == 8000 && player.current?.artwork != nil }
            player.togglePlayback()
            try await wait("library pause", player: player) { player.snapshot?.state == .paused }
            player.seek(seconds: 4)
            try await wait("library seek", player: player) { player.snapshot?.state == .paused && player.snapshot?.positionMs == 4000 }
            player.stop()
            try await wait("library stop", player: player) { player.snapshot?.state == .stopped }
            player.scanLibrary()
            try await wait("incremental rescan", player: player) { player.libraryState?.scan == nil && player.libraryState?.lastScan?.unchanged == 3 }
        }
        player.browseLibrary(mode: .tracks, text: "ТИХИЙ")
        try await wait("SQLite Unicode search", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.total == 1 && player.libraryPage?.tracks.first?.item.title == "Тихий берег" }
        if phase == "lost-access" {
            let id = player.libraryState!.roots[0].id
            await player.removeLibraryFolder(id)
            try await wait("remove index", player: player) { player.libraryState?.roots.isEmpty == true && player.libraryState?.totalTracks == 0 }
            guard FileManager.default.fileExists(atPath: fixtures.appendingPathComponent("Quiet Hours/tagged-first.flac").path) else { throw LibrarySmokeFailure(message: "Root removal deleted audio") }
        }
        print("M3 \(phase): bookmarks, SQLite catalog, albums/tracks, artwork thumbnail, Unicode query and lifecycle passed")
    }
}
