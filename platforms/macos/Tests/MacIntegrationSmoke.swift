import AppKit
import Foundation
import MediaPlayer
import os

struct MacSmokeFailure: Error { let message: String }
@MainActor
private final class QuietSystemSink: SystemMediaSession {
    func publish(_ state: PlaybackSnapshot) {}
    func clear() {}
}
private actor EmptyProfileCredentials: CredentialStore {
    private(set) var reads: [String] = []
    private(set) var mutations = 0

    func read(account: String) -> Data? { reads.append(account); return nil }
    func write(_ value: Data, account: String) { mutations += 1 }
    func remove(account: String) { mutations += 1 }
}
private actor FirstRunProcessProbe: SoloistProcessManaging {
    private(set) var launches = 0

    func launch(key: Data, name: String, initialVolume: UInt8, generation: UInt64, receive: @escaping @Sendable (Data) -> Void) { launches += 1 }
    func stop(generation: UInt64) {}
    func forgetSession(generation: UInt64) {}
    func sendControl(_ command: RemoteOutputCommand, wireID: UInt64) -> Bool { false }
}
private final class FirstRunOfflineProtocol: URLProtocol, @unchecked Sendable {
    private static let requests = OSAllocatedUnfairLock(initialState: 0)
    static var requestCount: Int { requests.withLock { $0 } }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        Self.requests.withLock { $0 += 1 }
        client?.urlProtocol(self, didFailWithError: URLError(.notConnectedToInternet))
    }
    override func stopLoading() {}
}
@main
struct MacIntegrationSmoke {
    @MainActor
    static func wait(_ label: String, player: PlayerStore, allowingError: Bool = false, condition: @MainActor () -> Bool) async throws {
        for _ in 0..<400 {
            if let error = player.errorMessage, !allowingError { throw MacSmokeFailure(message: "\(label): \(error)") }
            if condition() { return }
            try await Task.sleep(for: .milliseconds(25))
        }
        throw MacSmokeFailure(message: "Timed out: \(label), state=\(player.snapshot?.state.rawValue ?? "none"), position=\(player.snapshot?.positionMs ?? 0), duration=\(player.current?.durationMs ?? 0), pending=\(player.pendingCommands.count)")
    }
    static func check(_ value: Bool, _ message: String) throws { if !value { throw MacSmokeFailure(message: message) } }
    @MainActor
    static func escapePresentationChecks() throws {
        let presentations = NativeTransientPresentations()
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 300, height: 200),
                              styleMask: [.titled], backing: .buffered, defer: false)
        let otherWindow = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 300, height: 200),
                                   styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false; otherWindow.isReleasedWhenClosed = false
        defer { window.close(); otherWindow.close() }
        let owner = NSObject()
        var queueOpen = true
        var listening = true
        var calls: [String] = []
        presentations.set(owner: owner, window: window) { queueOpen = false; calls.append("dismiss") }
        func escape(editing: Bool = false, modal: Bool = false, flags: NSEvent.ModifierFlags = []) -> Bool {
            MacKeyboardShortcuts.handleEscape(keyCode: 53, flags: flags, editing: editing, modal: modal,
                dismissPresentation: { presentations.dismiss(in: window) },
                exitListening: { listening = false; calls.append("leave"); return true })
        }
        try check(!presentations.dismiss(in: otherWindow) && queueOpen,
                  "A queue presentation belongs only to its presenting window")
        try check(!escape(editing: true) && !escape(modal: true) && !escape(flags: .command)
                  && queueOpen && listening && calls.isEmpty,
                  "Text editing, native sheets and modified Escape keep their own dismissal")
        try check(escape() && !queueOpen && listening && calls == ["dismiss"],
                  "The first Escape dismisses the queue before leaving Now Playing")
        try check(escape() && !listening && calls == ["dismiss", "leave"],
                  "The second Escape leaves Now Playing after its presentation has closed")
        presentations.set(owner: owner, window: window) { calls.append("stale") }
        presentations.set(owner: owner, window: nil, dismiss: nil)
        try check(!presentations.dismiss(in: window) && calls == ["dismiss", "leave"],
                  "Removing a presentation cannot leave a stale Escape dismissal")
        presentations.set(owner: owner, window: window) { queueOpen = false; calls.append("ordinary") }
        queueOpen = true
        try check(MacKeyboardShortcuts.handleEscape(keyCode: 53, flags: [], editing: false, modal: false,
                    dismissPresentation: { presentations.dismiss(in: window) }, exitListening: { false })
                  && !queueOpen && calls.last == "ordinary",
                  "Escape also closes a queue popover outside Now Playing")
    }
    @MainActor
    static func copy(_ state: PlaybackSnapshot, active: Bool? = nil, capabilities: MediaCapabilities? = nil, provider: String? = nil,
                     connection: ProviderConnection? = nil, transport: TransportControls? = nil) -> PlaybackSnapshot {
        PlaybackSnapshot(revision: state.revision, state: state.state, positionMs: state.positionMs,
                         volume: state.volume, activeProvider: provider ?? state.activeProvider,
                         providerConnection: connection ?? state.providerConnection, capabilities: capabilities ?? state.capabilities,
                         queue: state.queue, transport: transport ?? state.transport, playbackSessionActive: active ?? state.playbackSessionActive,
                         session: state.session, queueIssues: state.queueIssues)
    }
    @MainActor
    static func freshProfile(_ directory: URL, fixture: URL) async throws {
        try check(!FileManager.default.fileExists(atPath: directory.path), "First-run profile must start with empty support storage")
        let credentials = EmptyProfileCredentials()
        let process = FirstRunProcessProbe()
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [FirstRunOfflineProtocol.self]
        let client = SpotifyOAuthClient(session: URLSession(configuration: config))
        var browserAttempts = 0

        func openProfile() async throws -> PlayerStore {
            let runtime = SoloistRuntime(credentials: credentials, owner: process, deviceName: "Auvra fresh profile fixture",
                                         installer: SoloistRuntimeInstaller(directory: directory.appendingPathComponent("PlaybackRuntime")))
            let player = PlayerStore(libraryDirectory: directory, loadSpotify: true, playbackRuntime: runtime,
                                     spotifyClientID: String(repeating: "a", count: 32), spotifyAuthorizationFactory: { status, access, disconnected in
                SpotifyAuthorization(credentials: credentials, client: client, status: status, access: access, disconnected: disconnected,
                                     openBrowser: { _ in browserAttempts += 1; return false }, callbackPort: nil)
            })
            try await wait("isolated startup without a Spotify account", player: player) {
                player.libraryState?.ready == true && player.snapshot?.session.ready == true
                    && player.spotifyAccount.phase == .disconnected && runtime.status.phase == .notConfigured
            }
            try check(player.spotifyAccount.issue == nil && runtime.status.processID == nil && player.spotifyProfileID == nil,
                      "Empty credentials did not settle into ordinary signed-out startup")
            return player
        }

        let player = try await openProfile()
        try check(player.libraryState?.totalTracks == 0 && player.current == nil && player.snapshot?.playbackSessionActive == false,
                  "A fresh profile opened with a preloaded or active selection")
        player.setVolume(0.05)
        player.openFile(fixture)
        try await wait("signed-out local playback reaches CoreAudio", player: player) {
            guard let stats = player.snapshot?.outputStatistics else { return false }
            return player.isPlaying && player.isLocalPlayback && (player.snapshot?.positionMs ?? 0) > 0
                && stats.callbacks > 0 && stats.mediaFrames > 0 && stats.nonzeroFrames > 0
        }
        let itemID = player.current?.id
        player.togglePlayback()
        try await wait("first-run local pause", player: player) { player.snapshot?.state == .paused }
        player.seek(seconds: 2.25)
        try await wait("first-run saved seek", player: player) {
            player.snapshot?.state == .paused && player.snapshot?.positionMs == 2250 && player.pendingCommands.isEmpty
        }
        await player.prepareToQuit()

        let restored = try await openProfile()
        try await Task.sleep(for: .milliseconds(300))
        try check(restored.current?.id == itemID && restored.snapshot?.session.restored == true
                  && restored.snapshot?.state == .paused && restored.snapshot?.positionMs == 2250
                  && restored.snapshot?.playbackSessionActive == false && (restored.snapshot?.outputStatistics?.nonzeroFrames ?? 0) == 0,
                  "Reopening a signed-out profile did not keep the same file and position silent on pause")
        restored.togglePlayback()
        try await wait("explicit resume after signed-out reopen", player: restored) {
            restored.isPlaying && (restored.snapshot?.positionMs ?? 0) > 2250 && (restored.snapshot?.outputStatistics?.nonzeroFrames ?? 0) > 0
        }
        await restored.prepareToQuit()

        let reads = await credentials.reads.sorted()
        let mutations = await credentials.mutations
        let launches = await process.launches
        try check(reads == ["spotify", "spotify", "spotify-soloist", "spotify-soloist"] && mutations == 0
                  && launches == 0 && browserAttempts == 0 && FirstRunOfflineProtocol.requestCount == 0,
                  "Signed-out startup or reopen attempted credential writes, browser, OAuth HTTP, or playback engine launch")
        print("Fresh macOS profile: empty Spotify/Soloist credentials read twice, no browser, OAuth HTTP or Soloist launch; real local CoreAudio, paused restore at 2250 ms, explicit resume passed")
    }
    @MainActor
    static func main() async throws {
        let directory = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
        let fixtures = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        try await freshProfile(directory.appendingPathComponent("fresh-profile", isDirectory: true),
                               fixture: fixtures.appendingPathComponent("Quiet Hours/tagged-first.flac"))
        if CommandLine.arguments.dropFirst(3).contains("--fresh-profile") { return }

        _ = NSApplication.shared
        NSApp.setActivationPolicy(.accessory)
        let player = PlayerStore(libraryDirectory: directory)
        let session = MediaPlayerSystemSession(player: player)
        let info = MPNowPlayingInfoCenter.default(); let commands = MPRemoteCommandCenter.shared()
        player.attachSystemMedia(session)
        try await wait("startup", player: player) { player.libraryState?.ready == true && player.snapshot?.session.ready == true && player.demoTracks.count == 3 }
        try check(!session.isPublishing && info.nowPlayingInfo == nil, "Idle app claimed system playback")
        try check(session.dispatch(.play) == .noActionableNowPlayingItem, "Idle app accepted a media key")
        player.play(player.demoTracks[0])
        try await wait("silent demo", player: player) { player.isPlaying }
        try check(player.snapshot?.playbackSessionActive == true && !session.isPublishing && info.nowPlayingInfo == nil, "Silent demo claimed Now Playing")
        try check(session.dispatch(.toggle) == .noActionableNowPlayingItem, "Silent demo accepted system commands")
        player.stop(); await player.addLibraryFolder(fixtures)
        try await wait("library", player: player) { player.libraryState?.totalTracks == 3 && player.libraryState?.scan == nil }
        player.browseLibrary(mode: .tracks)
        try await wait("tracks", player: player) { !player.isLoadingLibraryPage && player.libraryPage?.tracks.count == 3 }
        let tracks = player.libraryPage!.tracks.map(\.item).sorted { ($0.artwork == nil ? 1 : 0, $0.title) < ($1.artwork == nil ? 1 : 0, $1.title) }
        player.setVolume(0.2)
        player.send(.replaceQueue(items: tracks, start: 0, context: .userQueue))
        try await wait("local publication", player: player) { player.isPlaying && session.isPublishing && info.playbackState == .playing }
        try check(info.nowPlayingInfo?[MPMediaItemPropertyTitle] as? String == player.current?.title, "Wrong system title")
        let remotePause = await Task.detached { session.dispatch(.pause) }.value
        try check(remotePause == .success, "Background remote pause rejected")
        try await wait("remote pause", player: player) { player.snapshot?.state == .paused && info.playbackState == .paused }
        try await wait("system artwork", player: player) { info.nowPlayingInfo?[MPMediaItemPropertyArtwork] is MPMediaItemArtwork }
        let art = info.nowPlayingInfo![MPMediaItemPropertyArtwork] as! MPMediaItemArtwork
        try check(art.image(at: CGSize(width: 64, height: 64))?.size.width ?? 0 > 0, "System artwork callback returned no image")
        try check(info.nowPlayingInfo?[MPMediaItemPropertyArtist] as? String == "Auvra Sessions", "Wrong system artist")
        try check((info.nowPlayingInfo?[MPMediaItemPropertyPlaybackDuration] as? NSNumber)?.doubleValue == 8, "Wrong system duration")
        try check((info.nowPlayingInfo?[MPNowPlayingInfoPropertyPlaybackRate] as? NSNumber)?.doubleValue == 0, "Paused system clock kept running")
        try check(commands.playCommand.isEnabled && commands.changePlaybackPositionCommand.isEnabled && commands.nextTrackCommand.isEnabled, "Supported system controls were disabled")
        try check(session.dispatch(.seek(.nan)) == .noActionableNowPlayingItem, "NaN seek admitted")
        try check(session.dispatch(.seek(2.25)) == .success, "Seek rejected")
        try await wait("system seek", player: player) { player.snapshot?.positionMs == 2250 && player.snapshot?.state == .paused && player.pendingCommands.isEmpty && session.isPublishing && (info.nowPlayingInfo?[MPNowPlayingInfoPropertyElapsedPlaybackTime] as? NSNumber)?.doubleValue == 2.25 }
        try check(session.dispatch(.skip(-10)) == .success, "Backward seek rejected")
        try await wait("backward clamp", player: player) { player.snapshot?.positionMs == 0 && player.snapshot?.state == .paused && player.pendingCommands.isEmpty && session.isPublishing }
        try check(session.dispatch(.skip(10)) == .success, "Forward seek rejected")
        try await wait("forward clamp", player: player) { player.snapshot?.positionMs == 8000 && player.snapshot?.state == .paused && player.pendingCommands.isEmpty && session.isPublishing }
        try check(session.dispatch(.seek(1)) == .success, "Reset seek rejected")
        try await wait("reset seek", player: player) { player.snapshot?.positionMs == 1000 && player.snapshot?.state == .paused && player.pendingCommands.isEmpty && session.isPublishing }
        try check(session.dispatch(.play) == .success, "Resume rejected")
        try await wait("system resume", player: player) { player.isPlaying && (player.snapshot?.positionMs ?? 0) > 1000 && info.playbackState == .playing }
        _ = session.dispatch(.pause); try await wait("second pause", player: player) { player.snapshot?.state == .paused }
        let first = player.current!.id
        _ = session.dispatch(.next)
        try await wait("system next", player: player) { player.current?.id == tracks[1].id && player.snapshot?.state == .paused && session.isPublishing }
        _ = session.dispatch(.previous)
        try await wait("system previous", player: player) { player.current?.id == first && player.snapshot?.state == .paused && session.isPublishing }
        _ = session.dispatch(.shuffle(true)); _ = session.dispatch(.repeatMode(.all))
        try await wait("system modes", player: player) { player.snapshot?.queue.shuffle == true && player.snapshot?.queue.repeatMode == .all && commands.changeShuffleModeCommand.currentShuffleType == .items && commands.changeRepeatModeCommand.currentRepeatType == .all }
        player.adjustVolume(5); try await wait("volume upper clamp", player: player) { player.volume == 1 }
        player.adjustVolume(-5); try await wait("volume lower clamp", player: player) { player.volume == 0 }
        player.setVolume(0.2); player.seek(seconds: 2)
        try await wait("stable frame", player: player) { player.snapshot?.positionMs == 2000 && player.snapshot?.state == .paused && player.pendingCommands.isEmpty && session.isPublishing }
        let state = player.snapshot!
        try check(SystemMediaProjection(copy(state, active: false)) == nil, "Unplayed restored selection claimed the system")
        let noSystem = MediaCapabilities(play: true, pause: true, seek: true, volume: true, pcmAnalysis: false, systemMediaSession: false)
        try check(SystemMediaProjection(copy(state, capabilities: noSystem, provider: "arbitrary-remote")) == nil, "Source without system capability claimed the system")
        try check(SystemMediaProjection(copy(state, provider: "arbitrary-namespace")) != nil, "Policy branched on provider name")
        let unavailable = copy(state, capabilities: MediaCapabilities(play: false, pause: false, seek: false, volume: false, pcmAnalysis: false, systemMediaSession: false),
                               provider: "arbitrary-remote", connection: .unavailable,
                               transport: TransportControls(stop: false, next: false, previous: false, shuffle: false, repeat: false, queueEdit: false))
        try check(SystemMediaProjection(unavailable) == nil, "Unavailable external output claimed the system")
        let owned = SystemMediaProjection(unavailable, ownsOutput: true)
        try check(owned?.metadata.id == state.queue.current?.id && owned?.metadata.commands.isEmpty == true,
                  "Observed owned metadata or the command availability was lost during Web API unavailability")
        player.attachSystemMedia(QuietSystemSink()) // Isolate async lifetime checks from subsequent real snapshots.
        let stale = MediaPlayerSystemSession(player: player)
        stale.publish(state)
        try check(stale.dispatch(.seek(6)) == .success, "Fixture seek did not enter the native admission path")
        stale.clear()
        try await Task.sleep(for: .milliseconds(350))
        try check(player.snapshot?.positionMs == 2000 && !stale.isPublishing && info.nowPlayingInfo == nil, "Late command or artwork resurrected a cleared session")
        player.attachSystemMedia(session)
        try check(session.isPublishing, "Reattaching an eligible session did not publish")
        _ = session.dispatch(.stop)
        try await wait("system stop clears", player: player) { player.snapshot?.state == .stopped && !session.isPublishing && info.nowPlayingInfo == nil }
        try check(!commands.playCommand.isEnabled && !commands.changePlaybackPositionCommand.isEnabled, "Cleared command targets stayed enabled")
        player.send(.play); try await wait("restart publication", player: player) { player.isPlaying && session.isPublishing }
        player.send(.openFile(path: fixtures.appendingPathComponent("missing.wav").path))
        try await wait("file error clears", player: player, allowingError: true) { player.snapshot?.state == .error && !session.isPublishing && info.nowPlayingInfo == nil }
        player.errorMessage = nil
        player.send(.replaceQueue(items: tracks, start: 0, context: .userQueue))
        try await wait("error recovery publication", player: player) { player.isPlaying && session.isPublishing }
        player.play(player.demoTracks[0]); try await wait("source switch clears", player: player) { player.isPlaying && player.current?.id == player.demoTracks[0].id && !session.isPublishing && info.nowPlayingInfo == nil }
        try check(MacKeyboardShortcuts.routesSpace(keyCode: 49, flags: [], editing: false, modal: false), "Space transport missing")
        for flags: NSEvent.ModifierFlags in [.command, .control, .option, .shift] {
            try check(!MacKeyboardShortcuts.routesSpace(keyCode: 49, flags: flags, editing: false, modal: false), "Modified Space was intercepted")
        }
        try check(!MacKeyboardShortcuts.routesSpace(keyCode: 49, flags: [], editing: true, modal: false) && !MacKeyboardShortcuts.routesSpace(keyCode: 49, flags: [], editing: false, modal: true), "Text or dialog Space was intercepted")
        try escapePresentationChecks()
        try check(MacKeyboardShortcuts.navigation(keyCode: 124, flags: .command, editing: false, modal: false) == .next
                  && MacKeyboardShortcuts.navigation(keyCode: 123, flags: .command, editing: false, modal: false) == .previous,
                  "Command-arrow navigation must dispatch the matching direction")
        for key in [UInt16(123), 124] {
            for flags: NSEvent.ModifierFlags in [[], .option, [.command, .option], [.command, .control], [.command, .shift]] {
                try check(MacKeyboardShortcuts.navigation(keyCode: key, flags: flags, editing: false, modal: false) == nil,
                          "Plain arrows, seek chords and modified navigation remain with AppKit")
            }
            try check(MacKeyboardShortcuts.navigation(keyCode: key, flags: .command, editing: true, modal: false) == nil
                      && MacKeyboardShortcuts.navigation(keyCode: key, flags: .command, editing: false, modal: true) == nil,
                      "Command-arrow must retain native text editing and modal dispatch")
        }
        try check(MacKeyboardShortcuts.navigation(keyCode: 124, flags: [.command, .numericPad, .function, .capsLock], editing: false, modal: false) == .next
                  && MacKeyboardShortcuts.navigation(keyCode: 126, flags: .command, editing: false, modal: false) == nil,
                  "Incidental arrow flags must preserve Next and Command-Up must remain available for volume")
        await player.prepareToQuit()
        try check(!session.isPublishing && info.nowPlayingInfo == nil && session.dispatch(.play) == .noActionableNowPlayingItem, "Quit retained system ownership")
        print("M5 macOS integration: actual MediaPlayer metadata/artwork/state; background native command routing → Rust/CoreAudio; seek/skip/modes/volume; no claim for idle/restored/demo/remote; stale work, stop/error/switch/quit cleanup; text/dialog shortcut policy passed")
    }
}
