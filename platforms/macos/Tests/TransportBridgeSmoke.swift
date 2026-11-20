import Foundation
import Darwin

struct TransportSmokeFailure: Error { let message: String }
@main
struct TransportBridgeSmoke {
    @MainActor
    static func wait(_ label: String, _ player: PlayerStore, condition: @MainActor () -> Bool) async throws {
        for _ in 0..<400 {
            if let error = player.errorMessage { throw TransportSmokeFailure(message: "\(label): \(error)") }
            if condition() { return }
            try await Task.sleep(for: .milliseconds(25))
        }
        throw TransportSmokeFailure(message: "Timed out: \(label)")
    }
    static func check(_ value: Bool, _ message: String) throws { if !value { throw TransportSmokeFailure(message: message) } }
    @MainActor
    static func main() async throws {
        let phase = CommandLine.arguments[1]
        let directory = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        let fixtures = URL(fileURLWithPath: CommandLine.arguments[3], isDirectory: true)
        let expectation = directory.appendingPathComponent("expectation.json")
        let originalFile = directory.appendingPathComponent("Original.wav")
        let movedFile = directory.appendingPathComponent("Moved.wav")
        if phase == "lost-access" {
            let record = directory.appendingPathComponent("library-folders.json")
            var records = try JSONSerialization.jsonObject(with: Data(contentsOf: record)) as! [[String: Any]]
            records[0]["bookmark"] = Data("invalid M4 test bookmark".utf8).base64EncodedString()
            try JSONSerialization.data(withJSONObject: records).write(to: record, options: .atomic)
        }
        if phase == "folder-reconnect" {
            try FileManager.default.moveItem(at: directory.appendingPathComponent("Original Music"), to: directory.appendingPathComponent("Moved Music"))
            let record = directory.appendingPathComponent("library-folders.json")
            var records = try JSONSerialization.jsonObject(with: Data(contentsOf: record)) as! [[String: Any]]
            records[0]["bookmark"] = Data("M4 moved folder lost grant".utf8).base64EncodedString()
            try JSONSerialization.data(withJSONObject: records).write(to: record, options: .atomic)
        }
        if phase == "folder-restart" { try FileManager.default.moveItem(at: directory.appendingPathComponent("Moved Music"), to: directory.appendingPathComponent("Moved Again")) }
        if phase == "file-restart" { try FileManager.default.moveItem(at: originalFile, to: movedFile) }
        if phase == "file-missing" { try FileManager.default.removeItem(at: movedFile) }
        let player = PlayerStore(libraryDirectory: directory)
        try await wait("session ready", player) { player.snapshot?.session.ready == true && player.libraryState?.ready == true }
        try check(player.snapshot?.session.issue == nil, "Session storage failed")
        if phase == "prepare" || phase == "transport" {
            if player.libraryState?.roots.isEmpty == true { await player.addLibraryFolder(fixtures) }
            try await wait("fixture library", player) { player.libraryState?.totalTracks == 3 && player.libraryState?.scan == nil }
            player.browseLibrary(mode: .tracks, text: "")
            try await wait("fixture tracks", player) { !player.isLoadingLibraryPage && player.libraryPage?.tracks.count == 3 }
            let tracks = player.libraryPage!.tracks.map(\.item).sorted { $0.id < $1.id }
            player.send(.setRepeat(.off)); player.send(.setShuffle(false)); player.setVolume(0.2)
            player.send(.replaceQueue(items: tracks, start: 0, context: .search("M4 real transport")))
            try await wait("initial real output", player) { player.isPlaying && (player.snapshot?.positionMs ?? 0) > 80 }
            if phase == "prepare" {
                player.send(.pause)
                try await wait("initial pause", player) { player.snapshot?.state == .paused }
                player.playNext(tracks[2]); player.next()
                try await wait("paused Play Next", player) { player.current?.id == tracks[2].id && player.snapshot?.state == .paused }
                player.next()
                try await wait("paused context continuation", player) { player.current?.id == tracks[1].id && player.snapshot?.state == .paused }
                player.previous()
                try await wait("forward history", player) { player.current?.id == tracks[2].id && player.snapshot?.state == .paused }
                player.playNext(tracks[0]); player.send(.setShuffle(true)); player.send(.setRepeat(.all)); player.seek(seconds: 1)
                try await wait("prepared queue", player) { player.pendingCommands.isEmpty && player.snapshot?.positionMs == 1000 && player.snapshot?.queue.playNextCount == 1 && player.snapshot?.queue.forwardCount == 1 && player.snapshot?.queue.shuffle == true && player.snapshot?.queue.repeatMode == .all }
                let queue = player.snapshot!.queue
                let expected: [String: Any] = ["current": player.current!.id, "upcoming": queue.upcoming.map(\.id), "history": queue.history]
                try JSONSerialization.data(withJSONObject: expected).write(to: expectation, options: .atomic)
                await player.prepareToQuit()
                print("M4 prepare: real paused transitions, Play Next, forward history, shuffle/repeat/volume, exact queue and 1 s position flushed on close")
            } else {
                player.seek(seconds: Double(tracks[0].durationMs - 400) / 1000)
                try await wait("sample-clock EOF advanced", player) { player.isPlaying && player.current?.id == tracks[1].id }
                player.send(.setRepeat(.one)); player.seek(seconds: Double(tracks[1].durationMs - 400) / 1000)
                try await wait("repeat one restarted", player) { player.isPlaying && player.current?.id == tracks[1].id && (player.snapshot?.positionMs ?? UInt64.max) < 1000 && player.pendingCommands.isEmpty }
                player.next()
                try await wait("manual next bypasses repeat one", player) { player.isPlaying && player.current?.id == tracks[2].id }
                player.send(.setRepeat(.all)); player.seek(seconds: Double(tracks[2].durationMs - 400) / 1000)
                try await wait("repeat all restarted context", player) { player.isPlaying && player.current?.id == tracks[0].id }
                player.send(.setRepeat(.off)); player.send(.setShuffle(true))
                player.send(.replaceQueue(items: tracks, start: 0, context: .userQueue))
                try await wait("shuffled audio start", player) { player.isPlaying && player.current?.id == tracks[0].id && player.snapshot?.queue.upcoming.count == 2 && player.pendingCommands.isEmpty }
                let order = [player.current!.id] + player.snapshot!.queue.upcoming.map(\.id)
                try check(Set(order).count == 3, "Shuffle duplicated or lost a track")
                for expected in order.dropFirst() {
                    player.seek(seconds: Double(player.current!.durationMs - 400) / 1000)
                    try await wait("shuffled EOF", player) { player.isPlaying && player.current?.id == expected }
                }
                player.seek(seconds: Double(player.current!.durationMs - 400) / 1000)
                try await wait("final EOF stops", player) { player.snapshot?.state == .stopped && player.snapshot?.positionMs == player.current?.durationMs }
                await player.prepareToQuit()
                print("M4 transport: real sample-clock EOF, repeat one/all, manual next, shuffle permutation and final stop passed")
            }
        } else if phase == "restart" {
            let expected = try JSONSerialization.jsonObject(with: Data(contentsOf: expectation)) as! [String: Any]
            try await Task.sleep(for: .milliseconds(400))
            try check(player.snapshot?.state == .paused && player.snapshot?.positionMs == 1000, "Restore played or lost position")
            try check(player.current?.id == expected["current"] as? String && player.snapshot?.queue.upcoming.map(\.id) == expected["upcoming"] as? [String] && player.snapshot?.queue.history == expected["history"] as? [String], "Restored queue order/history changed")
            try check(player.snapshot?.queue.shuffle == true && player.snapshot?.queue.repeatMode == .all && abs(player.volume - 0.2) < 0.001, "Restored settings changed")
            player.send(.play)
            try await wait("resume at saved position", player) { player.isPlaying && (player.snapshot?.positionMs ?? 0) > 1000 }
            player.send(.pause); try await wait("pause restored audio", player) { player.snapshot?.state == .paused }
            await player.prepareToQuit()
            print("M4 restart: exact queue/history/settings restored paused; real playback resumed from saved 1 s")
        } else if phase == "checkpoint" {
            player.seek(seconds: 1.8)
            try await wait("durable crash checkpoint", player) { player.snapshot?.state == .paused && player.snapshot?.positionMs == 1800 && player.pendingCommands.isEmpty && player.snapshot?.session.saving == false }
            print("M4 checkpoint: durable paused position acknowledged; exiting without shutdown/deinit")
            fflush(stdout)
            _exit(0)
        } else if phase == "checkpoint-restart" {
            try await Task.sleep(for: .milliseconds(400))
            try check(player.snapshot?.state == .paused && player.snapshot?.positionMs == 1800, "Committed checkpoint was lost after abrupt exit")
            await player.prepareToQuit()
            print("M4 checkpoint restart: committed queue survived abrupt process exit and stayed paused")
        } else if phase == "lost-access" {
            try await wait("restored permission issue", player) { player.snapshot?.queueIssues.isEmpty == false && player.libraryState?.roots.first?.authorized == false }
            try check(player.current != nil && !player.canPlay && player.snapshot?.state == .paused, "Lost grant destroyed queue or permitted playback")
            let id = player.current!.id; let position = player.snapshot!.positionMs
            await player.addLibraryFolder(fixtures, replacing: player.libraryState!.roots[0].id)
            try await wait("grant revalidated", player) { player.snapshot?.queueIssues.isEmpty == true && player.canPlay }
            try check(player.current?.id == id && player.snapshot?.positionMs == position, "Reconnect changed selected item or position")
            player.send(.play); try await wait("reconnected real output", player) { player.isPlaying }
            player.send(.pause); try await wait("reconnected pause", player) { player.snapshot?.state == .paused }
            await player.prepareToQuit()
            print("M4 lost access: queue retained, playback blocked, reconnect revalidated without losing queue/position")
        } else if phase == "folder-prepare" {
            let music = directory.appendingPathComponent("Original Music")
            try FileManager.default.copyItem(at: fixtures, to: music)
            await player.addLibraryFolder(music)
            try await wait("movable folder scan", player) { player.libraryState?.totalTracks == 3 && player.libraryState?.scan == nil }
            player.browseLibrary(mode: .tracks)
            try await wait("movable folder page", player) { !player.isLoadingLibraryPage && player.libraryPage?.tracks.count == 3 }
            player.playLibrary(player.libraryPage!.tracks[0])
            try await wait("movable folder playback", player) { player.isPlaying }
            player.send(.pause); try await wait("movable folder pause", player) { player.snapshot?.state == .paused }
            player.seek(seconds: 1)
            try await wait("movable folder seek", player) { player.snapshot?.positionMs == 1000 && player.pendingCommands.isEmpty }
            let expected: [String: Any] = ["current": player.current!.id, "upcoming": player.snapshot!.queue.upcoming.map(\.id)]
            try JSONSerialization.data(withJSONObject: expected).write(to: expectation, options: .atomic)
            await player.prepareToQuit(); print("M4 folder prepare: whole queue and stable root mapping persisted")
        } else if phase == "folder-reconnect" || phase == "folder-restart" {
            let expected = try JSONSerialization.jsonObject(with: Data(contentsOf: expectation)) as! [String: Any]
            if phase == "folder-reconnect" {
                try check(!player.canPlay && player.snapshot?.queueIssues.isEmpty == false, "Moved inaccessible folder was silently authorized")
                player.setVolume(0.25)
                try await wait("blocked folder checkpoint", player) { player.pendingCommands.isEmpty && player.snapshot?.session.saving == false && abs(player.volume - 0.25) < 0.001 }
                await player.addLibraryFolder(directory.appendingPathComponent("Moved Music"), replacing: player.libraryState!.roots[0].id)
                try await wait("moved folder reconnect", player) { player.snapshot?.queueIssues.isEmpty == true && player.canPlay }
            }
            try check(player.snapshot?.state == .paused && player.snapshot?.positionMs == 1000 && player.current?.id == expected["current"] as? String && player.snapshot?.queue.upcoming.map(\.id) == expected["upcoming"] as? [String], "Folder move changed queue identity/order/position")
            let folder = directory.appendingPathComponent(phase == "folder-reconnect" ? "Moved Music" : "Moved Again").path
            for item in [player.current!] + player.snapshot!.queue.upcoming {
                if case .file(let path) = item.location { try check(path.hasPrefix(folder + "/"), "Folder locator not rebased") }
            }
            player.send(.play); try await wait("moved folder real output", player) { player.isPlaying && (player.snapshot?.positionMs ?? 0) > 1000 }
            player.send(.pause); try await wait("moved folder pause", player) { player.snapshot?.state == .paused }
            player.seek(seconds: 1); try await wait("moved folder checkpoint", player) { player.snapshot?.positionMs == 1000 && player.pendingCommands.isEmpty }
            await player.prepareToQuit(); print("M4 \(phase): whole-folder move retained queue identity/order/position and real output")
        } else if phase == "file-prepare" {
            try FileManager.default.copyItem(at: fixtures.appendingPathComponent("Singles/Auvra Mono Check.wav"), to: originalFile)
            player.openFile(originalFile)
            try await wait("standalone file", player) { player.isPlaying && player.current?.durationMs == 3000 }
            player.send(.pause); try await wait("standalone pause", player) { player.snapshot?.state == .paused }
            player.seek(seconds: 1.234)
            try await wait("standalone saved position", player) { player.snapshot?.positionMs == 1234 && player.pendingCommands.isEmpty }
            await player.prepareToQuit()
            print("M4 standalone: native file bookmark and 1234 ms position persisted")
        } else if phase == "file-restart" {
            try check(player.snapshot?.state == .paused && player.snapshot?.positionMs == 1234 && player.snapshot?.queueIssues.isEmpty == true, "Moved standalone file failed to restore")
            if case .file(let path) = player.current?.location { try check(path == movedFile.path, "Moved bookmark did not rebase queue locator") }
            else { throw TransportSmokeFailure(message: "Lost file locator") }
            player.send(.play); try await wait("moved file real output", player) { player.isPlaying && (player.snapshot?.positionMs ?? 0) > 1234 }
            player.send(.pause); try await wait("moved file pause", player) { player.snapshot?.state == .paused }
            await player.prepareToQuit()
            print("M4 standalone restart: moved bookmark refreshed, identity/position retained, real audio resumed")
        } else if phase == "file-missing" {
            try check(player.snapshot?.queueIssues.first?.error.code == "file_missing" && !player.canPlay && player.current != nil, "Missing file did not retain a blocked queue item")
            player.send(.clearQueue); player.send(.setVolume(0.42)); player.send(.setRepeat(.one))
            await player.prepareToQuit() // No receipt wait: close must drain all admitted commands.
            print("M4 missing file: unavailable queue retained; immediate close drained clear/settings and final checkpoint")
        } else if phase == "empty" {
            try check(player.snapshot?.state == .idle && player.current == nil && player.snapshot?.queue.upcoming.isEmpty == true, "Cleared queue resurrected")
            try check(abs(player.volume - 0.42) < 0.001 && player.snapshot?.queue.repeatMode == .one, "Final admitted settings were lost on close")
            await player.prepareToQuit(); print("M4 empty restart: cleared queue and final admitted settings persisted")
        } else if phase == "ended" {
            try check(player.snapshot?.state == .stopped && player.snapshot?.positionMs == player.current?.durationMs, "Completed queue did not restore stopped at EOF")
            player.send(.play); try await wait("completed queue restart", player) { player.isPlaying && (player.snapshot?.positionMs ?? UInt64.max) < 1000 }
            player.send(.pause); try await wait("completed queue pause", player) { player.snapshot?.state == .paused }
            await player.prepareToQuit(); print("M4 completed restart: EOF restored stopped; explicit Play restarted selected track")
        } else { throw TransportSmokeFailure(message: "Unknown phase") }
    }
}
