import AppKit
import QuartzCore
import SwiftUI
import MediaPlayer
import Observation

private struct Failure: Error { let message: String }
private final class UIObservationCount: @unchecked Sendable {
    private let lock = NSLock()
    private var value = 0
    func record() { lock.lock(); defer { lock.unlock() }; value += 1 }
    var count: Int { lock.lock(); defer { lock.unlock() }; return value }
}
private actor FixtureCredentials: CredentialStore {
    func read(account: String) -> Data? { Data("original-fixture-key".utf8) }
    func write(_ value: Data, account: String) {}
    func remove(account: String) {}
}
private actor FixtureProcess: SoloistProcessManaging {
    var receive: (@Sendable (Data) -> Void)?
    private var previousReceive: (@Sendable (Data) -> Void)?
    private(set) var initialVolumes: [UInt8] = []
    private(set) var sentControlCount = 0
    private(set) var activationRequests: [UInt64] = []
    private(set) var cancelledActivations: [UInt64] = []
    private var holdActivationResults = false
    private var rejectControl = false
    private var omitControlResult = false
    private var pauseNextStop = false
    private var stopped: CheckedContinuation<Void, Never>?
    let clockSupported: Bool
    let controlSupported: Bool
    init(clockSupported: Bool = false, controlSupported: Bool = false) {
        self.clockSupported = clockSupported; self.controlSupported = controlSupported
    }
    func launch(key: Data, name: String, initialVolume: UInt8, generation: UInt64, receive: @escaping @Sendable (Data) -> Void) {
        previousReceive = self.receive
        self.receive = receive
        initialVolumes.append(initialVolume)
        receive(Data("{\"event\":\"started\",\"pid\":12345,\"clock_protocol\":\(clockSupported ? 1 : 0),\"control_protocol\":\(controlSupported ? 1 : 0)}\n{\"event\":\"auth\",\"logged_in\":true}\n".utf8))
    }
    func stop(generation: UInt64) async {
        if pauseNextStop {
            pauseNextStop = false
            await withCheckedContinuation { stopped = $0 }
        }
    }
    func holdNextStop() { pauseNextStop = true }
    func isStopHeld() -> Bool { stopped != nil }
    func releaseStop() { stopped?.resume(); stopped = nil }
    func forgetSession(generation: UInt64) {}
    func rejectNextControl() { rejectControl = true }
    func omitNextControlResult() { omitControlResult = true }
    func holdActivationACKs() { holdActivationResults = true }
    func sendActivation(id: UInt64, expiresAtUptimeMs: UInt64, expiresAtContinuousNs: UInt64, generation: UInt64) -> Bool {
        guard controlSupported else { return false }
        activationRequests.append(id)
        if !holdActivationResults { acknowledgeActivation(id) }
        return true
    }
    func cancelActivation(id: UInt64, generation: UInt64) { cancelledActivations.append(id) }
    func acknowledgeActivation(_ id: UInt64? = nil, accepted: Bool = true) {
        guard let id = id ?? activationRequests.last else { return }
        receive?(Data("{\"event\":\"activation_result\",\"id\":\(id),\"accepted\":\(accepted)}\n".utf8))
    }
    func sendControl(_ command: RemoteOutputCommand, wireID: UInt64) -> Bool {
        guard controlSupported else { return false }
        sentControlCount += 1
        let accepted = !rejectControl
        rejectControl = false
        if omitControlResult { omitControlResult = false; return true }
        receive?(Data("{\"event\":\"control_result\",\"id\":\(wireID),\"accepted\":\(accepted)}\n".utf8))
        return true
    }
    func exitRuntime() { receive?(Data("{\"event\":\"exited\",\"code\":10}\n".utf8)) }
    func event(_ json: String) { receive?(Data((json + "\n").utf8)) }
    func previousEvent(_ json: String) { previousReceive?(Data((json + "\n").utf8)) }
    func clock(_ id: String, sequence: UInt64, state: String = "playing", rate: Double = 1.5, active: Bool = true,
               position: UInt64 = 2000, queueEpoch: UInt64? = nil, sourcePositionTimestampMs: Double? = nil,
               context: String? = nil, complete: Bool = false, uptime: Double? = nil) {
        var clock: [String: Any] = ["media_id":id,"sequence":sequence,"state":state,"position_ms":position,"rate":rate,"active":active,"uptime":uptime ?? ProcessInfo.processInfo.systemUptime]
        if let queueEpoch { clock["queue_epoch"] = queueEpoch }
        if let sourcePositionTimestampMs { clock["source_position_timestamp_ms"] = sourcePositionTimestampMs }
        if let context {
            clock["metadata"] = ["title":"Original clock fixture", "artists":["Original fixture artist"],
                                 "duration_ms":240_000, "context_uri":context] as [String: Any]
        }
        if complete {
            clock["actions"] = ["play":state == "paused", "pause":state == "playing", "seek":true,
                                "next":true, "previous":true, "shuffle":true, "repeat":true, "queue_add":true]
        }
        let value: [String: Any] = ["event":"clock","clock":clock]
        var data = try! JSONSerialization.data(withJSONObject: value); data.append(10); receive?(data)
    }
}
private enum DetailFixtureMode: Equatable { case automatic, manual, reject }
private final class FixtureConnection: PlayerConnection, @unchecked Sendable {
    let receive: PlayerEventReceiver
    let items: [MediaItem]
    private let lock = NSLock()
    private var commands: [PlayerCommand] = []
    private var requestIDs: [UInt64] = []
    private var closeCount = 0
    private let mainMode: DetailFixtureMode
    private let details: DetailFixtureMode
    private let rejectResume: Bool
    private let acceptsRestoreCancellation: Bool
    private let rejectConfirmedRestoredPause: Bool
    private let holdOutputClockResults: Bool
    init(_ receive: @escaping PlayerEventReceiver, items: [MediaItem], details: DetailFixtureMode = .automatic, rejectResume: Bool = false, mainMode: DetailFixtureMode = .automatic, acceptsRestoreCancellation: Bool = false, rejectConfirmedRestoredPause: Bool = false, holdOutputClockResults: Bool = false) { self.receive = receive; self.items = items; self.details = details; self.rejectResume = rejectResume; self.mainMode = mainMode; self.acceptsRestoreCancellation = acceptsRestoreCancellation; self.rejectConfirmedRestoredPause = rejectConfirmedRestoredPause; self.holdOutputClockResults = holdOutputClockResults }
    func send(_ command: PlayerCommand, requestId: UInt64) {
        lock.lock(); commands.append(command); requestIDs.append(requestId); lock.unlock()
        DispatchQueue.main.async {
            if self.holdOutputClockResults, case .remote(_, .outputClock(let clock)) = command, clock != nil { return }
            if case .remote(_, .query(let query)) = command, !query.kind.isDetail, self.mainMode == .manual { return }
            if case .remote(_, .query(let query)) = command, !query.kind.isDetail, self.mainMode == .reject {
                self.receive(.commandResult(CommandOutcome(requestId: requestId,error: PlayerIssue(code: "busy",message: "Original catalog admission rejection")))); return
            }
            if case .remote(_, .control(.resumeOnDevice(_,_,_,_,_))) = command, self.rejectResume {
                self.receive(.commandResult(CommandOutcome(requestId: requestId,error: PlayerIssue(code: "invalid_command",message: "Original saved selection rejection")))); return
            }
            if case .cancelRestoredResume = command {
                let error = self.acceptsRestoreCancellation ? nil : PlayerIssue(code: "unsupported_capability", message: "Original Resume write already started")
                self.receive(.commandResult(CommandOutcome(requestId: requestId, error: error))); return
            }
            if case .pauseConfirmedRestoredResume = command, self.rejectConfirmedRestoredPause {
                self.receive(.commandResult(CommandOutcome(requestId: requestId, error: PlayerIssue(code: "unsupported_capability", message: "Original Resume output is no longer current")))); return
            }
            if case .remote(_, .query(let query)) = command, query.kind.isDetail, self.details == .reject {
                self.receive(.commandResult(CommandOutcome(requestId: requestId, error: PlayerIssue(code: "rate_limited", message: "Original detail admission rejection")))); return
            }
            self.receive(.commandResult(CommandOutcome(requestId: requestId, error: nil)))
            if case .remote(_, .query(let query)) = command {
                if query.kind.isDetail && self.details == .manual { return }
                if query.kind == .artistTopTracks {
                    self.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: query.queryId,
                        offset: 0, total: 0, items: [], collections: [], issue: PlayerIssue(code: "unsupported_capability", message: "Original development-mode chart restriction"))))
                    return
                }
                if query.kind == .artistSelectedTracks {
                    self.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: query.queryId,
                        offset: 0, total: UInt64(self.items.count), items: self.items, collections: [], issue: nil)))
                    return
                }
                if [.recent, .homeRecent].contains(query.kind), let item = self.items.first {
                    let history = [item, item] + self.items.dropFirst().prefix(8)
                    let plays = [
                        RecentPlay(itemIndex: 0, playedAt: "2026-11-05T16:18:00.250Z", contextUri: "spotify:album:" + String(repeating: "a", count: 22)),
                        RecentPlay(itemIndex: 1, playedAt: "2026-11-05T16:13:30Z", contextUri: "spotify:playlist:" + String(repeating: "p", count: 22)),
                        RecentPlay(itemIndex: 2, playedAt: "2026-11-05T16:05:00Z", contextUri: nil)
                    ].filter { Int($0.itemIndex) < history.count }
                    self.receive(.remotePage(RemotePage(detail: query.kind.isDetail,
                        nextCursor: query.kind == .recent && query.offset == 0 ? "1793894610000" : nil, recentPlays: plays,
                        provider: "spotify", queryId: query.queryId, offset: query.offset, total: UInt64(history.count),
                        items: history, collections: [], issue: nil)))
                    return
                }
                let entity: RemoteEntityKind? = switch query.kind {
                case .playlists, .searchPlaylists, .playlistInfo, .playlistImages, .homePlaylists, .homeRadio: .playlist
                case .searchAlbums, .savedAlbums, .artistAlbums, .album, .homeArtistAlbums: .album
                case .searchArtists, .followedArtists, .topArtists, .artist, .homeTopArtists: .artist
                case .searchShows, .savedShows, .show: .show
                case .searchAudiobooks, .savedAudiobooks, .audiobook: .audiobook
                case .profile: .profile
                default: nil
                }
                let singleton = query.kind.isDetail && !RemoteCatalogKind.homeKinds.contains(query.kind) && ![.homeArtistAlbums, .artistAlbums].contains(query.kind)
                let identifier = query.kind == .profile ? "original-listener" : singleton ? query.text : String(repeating: "p",count: 22)
                let entities: [RemoteEntityKind] = query.kind == .searchAll
                    ? [.album, .artist, .playlist, .show, .audiobook] : entity.map { [$0] } ?? []
                let collections = entities.enumerated().map { index, kind in
                    let id: String
                    let title: String
                    if query.kind == .homeArtistAlbums {
                        id = String(repeating: "n", count: 22)
                        title = "Original discovery album"
                    } else {
                        id = query.kind == .searchAll ? String(repeating: "\(index)", count: 22) : identifier
                        title = query.kind == .searchAll && kind == .artist ? query.text : "Original \(kind.title) — длинное название"
                    }
                    return RemoteCollection(id: id,title: title,url: "https://open.spotify.com/\(kind.rawValue)/" + id,artwork: nil,isPublic: false,kind: kind,subtitle: "Original creator",description: "Original full metadata fixture",ownerId: kind == .playlist ? "original-listener" : nil,collaborative: kind == .playlist ? false : nil)
                }
                let items: [MediaItem]
                if query.kind == .searchAll || query.kind == .searchEpisodes {
                    let id = String(repeating: "e", count: 22)
                    let uri = "spotify:episode:" + id
                    let episode = MediaItem(id: uri, title: "Original episode · How sound travels",
                                            artists: ["Original creator"], album: nil, durationMs: 120000, artwork: nil,
                                            capabilities: .init(play: true, pause: true, seek: true, volume: true, pcmAnalysis: false, systemMediaSession: false),
                                            location: .remote(uri: uri, url: "https://open.spotify.com/episode/" + id, service: "Spotify"))
                    items = query.kind == .searchAll ? self.items + [episode] : [episode]
                } else if collections.isEmpty && singleton, let original = self.items.first {
                    items = [MediaItem(id: "spotify:\(query.kind.rawValue):\(query.text)",title: original.title,artists: original.artists,album: original.album,durationMs: original.durationMs,artwork: original.artwork,capabilities: original.capabilities,location: original.location)]
                } else { items = collections.isEmpty ? self.items : [] }
                let relatedKinds: [RemoteEntityKind] = switch query.kind { case .track: [.album,.artist]; case .episode: [.show]; case .chapter: [.audiobook]; default: [] }
                let related = relatedKinds.enumerated().map { index, kind in
                    RemoteCollection(id: String(repeating: index == 0 ? "b" : "c", count: 22),title: "Original related \(kind.title)",url: "https://open.spotify.com/\(kind.rawValue)/" + String(repeating: index == 0 ? "b" : "c",count: 22),artwork: nil,kind: kind)
                }
                self.receive(.remotePage(RemotePage(positionEdits: query.kind == .playlistItems,relatedCollections: related,detail: query.kind.isDetail, itemPositions: Array(items.indices).map {query.offset + UInt32($0)},provider: "spotify",queryId: query.queryId,offset: query.offset,total: query.kind.isDetail ? 1 : 2,items: items,collections: collections,issue: nil)))
            }
        }
    }
    func lastRequestID() -> UInt64 { lock.lock(); defer { lock.unlock() }; return requestIDs.last! }
    func lastRequestID(matching predicate: (PlayerCommand) -> Bool) -> UInt64? {
        lock.lock(); defer { lock.unlock() }
        return commands.lastIndex(where: predicate).map { requestIDs[$0] }
    }
    func recorded() -> [PlayerCommand] { lock.lock(); defer { lock.unlock() }; return commands }
    func closedCount() -> Int { lock.lock(); defer { lock.unlock() }; return closeCount }
    func close(completion: @escaping @MainActor @Sendable () -> Void) {
        lock.lock(); closeCount += 1; lock.unlock()
        DispatchQueue.main.async { completion() }
    }
}
private final class SnapshotWindow: NSWindow {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}
@MainActor
private final class Checks: NSObject, NSApplicationDelegate {
    let evidence: URL
    var passed: [String] = []
    var connections: [FixtureConnection] = []
    var revision: UInt64 = 1
    private var inputMonitor: Any?
    private var ignoredInputEvents = 0
    init(_ evidence: URL) { self.evidence = evidence }
    func applicationDidFinishLaunching(_ notification: Notification) {
        // Raster fixtures are model-driven. Do not let input intended for another
        // app edit their temporary focused fields; never record input contents.
        inputMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown,.keyUp,.leftMouseDown,.leftMouseUp,.rightMouseDown,.rightMouseUp,.scrollWheel]) { [weak self] _ in
            MainActor.assumeIsolated { self?.ignoredInputEvents += 1 }
            return nil
        }
        Task { @MainActor in
            defer { if let inputMonitor { NSEvent.removeMonitor(inputMonitor) } }
            do { try await run(); save(nil); print("Player UI regression passed: \(passed.count) checks; system ownership, actual transport admission, pending/cancel/stale state, native responsive screens."); NSApp.terminate(nil) }
            catch { save(String(describing: error)); print("Player UI regression failed: \(error)"); exit(1) }
        }
    }
    func save(_ failure: String?) {
        let data = try! JSONSerialization.data(withJSONObject: ["checks":passed,"failures":failure.map { [$0] } ?? [],"ignoredInputEvents":ignoredInputEvents], options: [.prettyPrinted,.sortedKeys])
        try! data.write(to: evidence.appendingPathComponent("ui-regression.json"))
    }
    func check(_ value: Bool, _ message: String) throws { guard value else { throw Failure(message: message) }; passed.append(message) }
    func wait(_ condition: () -> Bool) async throws {
        for _ in 0..<100 { if condition() { return }; try await Task.sleep(for: .milliseconds(20)) }
        throw Failure(message: "Fixture event timed out")
    }
    var capabilities: MediaCapabilities { .init(play: true,pause: true,seek: true,volume: true,pcmAnalysis: false,systemMediaSession: false) }
    func track(_ id: String = "a", playable: Bool = true, title: String = "A long original track title for a compact window — проверка длинного названия") -> MediaItem {
        MediaItem(id: "spotify:track:\(String(repeating: id, count: 22))", title: title, artists: ["Original fixture artist", "Второй автор"], album: "Original album",durationMs: 240000,artwork: nil,
                  capabilities: .init(play: playable,pause: true,seek: true,volume: true,pcmAnalysis: false,systemMediaSession: false,librarySave: true),
                  location: .remote(uri: "spotify:track:\(String(repeating: id, count: 22))",url: "https://open.spotify.com/track/\(String(repeating: id, count: 22))",service: "Spotify"))
    }
    func frame(_ item: MediaItem, device: String = "owned-output", playing: Bool = true, context: String? = nil, position: UInt64 = 1250,
               connection: ProviderConnection = .connected, ownedOutputObserved: Bool? = nil, active: Bool = true, restricted: Bool = false,
               volume: Float = 0.2, previous: Bool = false, shuffle: Bool = false, repeatMode: RepeatMode = .off,
               positionRate: Double? = nil, pauseCapability: Bool = true) -> RemoteState {
        let device = RemoteDevice(id: device,name: device == "owned-output" ? "Auvra fixture" : "External speaker",deviceType: "Computer",volume: 0.2,isActive: active,isRestricted: restricted)
        return RemoteState(ownedOutputObserved: ownedOutputObserved,provider: "spotify",connection: connection,authenticationRequired: false,issue: nil,retryAfterSeconds: nil,devices: [device],playback: RemotePlayback(positionRate: positionRate,contextUri: context,state: playing ? .playing : .paused,positionMs: position,volume: volume,current: item,upcoming: [],shuffle: shuffle,repeat: repeatMode,capabilities: .init(play: true,pause: pauseCapability,seek: true,volume: true,pcmAnalysis: false,systemMediaSession: false),transport: .init(stop: false,next: true,previous: previous,shuffle: true,repeat: true,queueEdit: false),device: device))
    }
    func project(_ driver: FixtureConnection, _ incoming: RemoteState, acknowledge: Bool = true, analysis: Bool = false,
                 deviceQueue: DeviceQueueDetails? = nil, upcoming: [MediaItem]? = nil, analysisStatus: String? = nil,
                 spatialStatus: String? = nil, analysisRevokedThrough: UInt64? = nil, restored: RestoredRemote? = nil) {
        var state = incoming
        if acknowledge, state.controlResult == nil, let operation = driver.recorded().reversed().compactMap({ command -> UInt64? in if case .remote(_, .control(let control)) = command { return control.operationId }; return nil }).first {
            state.controlResult = RemoteControlResult(operationId: operation,error: nil)
        }
        driver.receive(.remoteState(state)); revision += 1
        let frame = state.playback!
        driver.receive(.snapshot(PlaybackSnapshot(hostAnalysisStatus: analysisStatus, hostSpatialStatus: spatialStatus, hostAnalysisRevokedThrough: analysisRevokedThrough, enhancement: frame.enhancement,playbackRate: frame.positionRate,restoredRemote: restored,revision: revision,state: frame.state,positionMs: frame.positionMs,volume: frame.volume,activeProvider: "spotify",providerConnection: state.connection,capabilities: analysis ? .init(play: true,pause: true,seek: true,volume: true,pcmAnalysis: true,systemMediaSession: false) : frame.capabilities,
            queue: QueueState(deviceQueue: deviceQueue,revision: revision,context: .userQueue,current: frame.current,upcoming: upcoming ?? frame.upcoming,history: [],repeatMode: frame.repeat,shuffle: frame.shuffle,playNextCount: 0,forwardCount: 0),transport: frame.transport,playbackSessionActive: true,
            session: SessionState(enabled: false,ready: true,restored: false,saving: false,issue: nil),queueIssues: [])))
    }
    func render(_ root: some View, name: String, size: NSSize, colorScheme: ColorScheme = .dark, ready: @escaping @MainActor () -> Bool = { true }) async throws {
        let previous = UserDefaults.standard.object(forKey: AppAppearance.preferenceKey)
        UserDefaults.standard.set(colorScheme == .light ? "light" : "dark",forKey: AppAppearance.preferenceKey)
        defer { if let previous { UserDefaults.standard.set(previous,forKey: AppAppearance.preferenceKey) } else { UserDefaults.standard.removeObject(forKey: AppAppearance.preferenceKey) } }
        // Static evidence must include the whole settled surface. Production
        // motion is exercised separately in the running app.
        let host = NSHostingView(rootView: AnyView(root.environment(\.colorScheme, colorScheme)
            .transaction { $0.disablesAnimations = true }))
        let window = SnapshotWindow(contentRect: NSRect(origin: .zero,size: size),styleMask: [.titled,.closable,.resizable],backing: .buffered,defer: false)
        window.title = "Auvra UI regression · \(name)"; window.contentView = host; window.isReleasedWhenClosed = false
        window.appearance = NSAppearance(named: colorScheme == .light ? .aqua : .darkAqua)
        window.center(); window.orderFront(nil)
        defer { window.close() }
        try await Task.sleep(for: .milliseconds(600))
        try await wait(ready)
        host.layoutSubtreeIfNeeded()
        host.display()
        CATransaction.flush()
        if name == "lyrics-compact" || name == "lyrics-wide" {
            func lyricObservers(_ view: NSView) -> [LyricsScrollObservationView] {
                (view as? LyricsScrollObservationView).map { [$0] } ?? view.subviews.flatMap(lyricObservers)
            }
            let observers = lyricObservers(host)
            try check(observers.count == 1 && observers.allSatisfy { $0.bounds.intersection($0.visibleRect).width > 1 && $0.bounds.intersection($0.visibleRect).height > 1 }, "\(name): the native scroll observer occupies the visible lyric region")
        }
        // SwiftUI's AX tree is lazy and owned by the system AX client. This
        // in-process check verifies native layout/raster output; CUA separately
        // verifies accessibility labels and the number of source marks.
        try check(host.frame.width <= size.width + 1 && host.frame.height <= size.height + 40,
                  "\(name): fits requested viewport plus native titlebar (\(host.frame.size) in \(size))")
        guard let bitmap = host.bitmapImageRepForCachingDisplay(in: host.bounds) else { throw Failure(message: "Native render unavailable") }
        window.effectiveAppearance.performAsCurrentDrawingAppearance { host.cacheDisplay(in: host.bounds,to: bitmap) }
        // A fitted window can still be a blank raster (for example a suspended
        // future-only TimelineView). Sample real pixels as well as checking size.
        var colors = Set<UInt32>()
        var brightness = 0.0, sampled = 0.0
        for y in stride(from: 0,to: bitmap.pixelsHigh,by: max(1,bitmap.pixelsHigh / 48)) {
            for x in stride(from: 0,to: bitmap.pixelsWide,by: max(1,bitmap.pixelsWide / 64)) {
                if let color = bitmap.colorAt(x: x,y: y)?.usingColorSpace(.sRGB) {
                    let channels = [color.redComponent,color.greenComponent,color.blueComponent,color.alphaComponent]
                    brightness += (color.redComponent + color.greenComponent + color.blueComponent) / 3; sampled += 1
                    colors.insert(channels.reduce(UInt32(0)) { ($0 << 8) | UInt32(min(255,max(0,$1 * 255)).rounded()) })
                }
            }
        }
        try check(colors.count > 1,"\(name): native raster contains visible content")
        if name == "light-now-playing-solid" {
            let scale = Double(bitmap.pixelsWide) / host.bounds.width
            func stripBrightness(_ fraction: Double) -> Double {
                let x = Int(Double(bitmap.pixelsWide) * fraction)
                let samples = stride(from: 48, through: 68, by: 2).compactMap { offset in
                    bitmap.colorAt(x: x, y: bitmap.pixelsHigh - Int(Double(offset) * scale))?
                        .usingColorSpace(.sRGB)?.redComponent
                }.sorted()
                return samples.isEmpty ? 0 : samples[samples.count / 2]
            }
            try check(stripBrightness(0.50) - stripBrightness(0.65) > 0.25,
                      "The solid Now Playing islands leave Frost visible between them")
        }
        if name == "light-player-bar-solid" {
            let scale = Double(bitmap.pixelsWide) / host.bounds.width
            let row = bitmap.pixelsHigh - Int(25 * scale)
            let samples = [2, 30, 50, 70, 98].map { x in
                bitmap.colorAt(x: bitmap.pixelsWide * x / 100, y: row)!.usingColorSpace(.sRGB)!.redComponent
            }
            try check(samples.max()! - samples.min()! < 0.025, "The normal player has one continuous lower bar")
        }
        if name.hasPrefix("light-") && !["light-now-playing","light-now-playing-solid","light-lyrics"].contains(name) { try check(brightness / max(1,sampled) > 0.6,"\(name): light appearance reaches the actual native raster") }
        guard let png = bitmap.representation(using: .png,properties: [:]) else { throw Failure(message: "Native PNG unavailable") }
        try png.write(to: evidence.appendingPathComponent(name + ".png"))
        // Remove the rendered view while the host can still deliver SwiftUI's
        // disappearance. Closing a retained background window can defer it.
        host.rootView = AnyView(EmptyView())
        host.layoutSubtreeIfNeeded()
        try await Task.sleep(for: .milliseconds(50))
    }
    func lyricAppearanceChecks() async throws {
        let ripple = LyricsRowRipple(target: 4, startedAt: 10)
        func offset(_ index: Int, _ time: Double, _ ripples: [LyricsRowRipple] = []) -> CGFloat {
            LyricsRowRipple.offset(for: index, at: time, ripples: [ripple] + ripples)
        }
        try check(offset(3, 10.2) < offset(4, 10.2) && offset(4, 10.2) < offset(5, 10.2)
                  && offset(4, 10) == 0 && offset(9, 10.61) == 0,
                  "Adjacent lyrics lag smoothly in order and land on the native row positions")
        let second = LyricsRowRipple(target: 5, startedAt: 10.15)
        let third = LyricsRowRipple(target: 6, startedAt: 10.25)
        let clearance = stride(from: 10.0, through: 10.85, by: 0.001).flatMap { time in
            (0..<10).map { index in
                LyricsRowRipple.rowSpacing + offset(index + 1, time, [second, third]) - offset(index, time, [second, third])
            }
        }.min()!
        try check(clearance >= 4 && offset(5, 10.15, [second]) == offset(5, 10.15),
                  "Rapid cues preserve motion continuity and leave a safe gap even beside a newly loaded wrapped row")
        let cue = LyricsLine(text: "The next line", startMs: 3000, endMs: 4500, words: [])
        let cueDocument = LyricsDocument(lines: [cue], instrumental: false, precision: "line")
        try check(cueDocument.upcomingLine(at: 1899) == nil && cueDocument.upcomingLine(at: 1900) == 0
                  && cueDocument.focusedLine(at: 1900) == nil && cueDocument.focusedLine(at: 3000) == 0,
                  "An upcoming lyric can enter visual focus during a short gap without starting its sung timing early")
        let beforeCue = LyricsPresentationClock.time(position: 3000, correction: 0, playing: true)
        let atCue = LyricsPresentationClock.time(position: 3075, correction: 0, playing: true)
        let paused = LyricsPresentationClock.time(position: 2750, correction: 250, playing: false)
        try check(cueDocument.focusedLine(at: beforeCue) == nil && cueDocument.focusedLine(at: atCue) == 0
                  && cueDocument.focusedLine(at: paused, paused: true) == 0 && cueDocument.precedingLine(at: 5000) == 0,
                  "The 75 ms presentation delay composes with timing correction and preserves exact paused seeks and gap anchoring")

        let text = "A steady light"
        let timed = LyricsLine(text: text, startMs: 2000, endMs: 4000, words: [])
        let plain = LyricsLine(text: text, startMs: nil, endMs: nil, words: [])
        let wordTimed = LyricsLine(text: text, startMs: 2000, endMs: 4000, words: [
            LyricsWord(text: "A steady ", startMs: 2000, endMs: 2800, endEstimated: false),
            LyricsWord(text: "light", startMs: 2800, endMs: 3800, endEstimated: false)
        ])
        let following = LyricsLine(text: "The next line", startMs: 5000, endMs: 7000, words: [])
        let wordDocument = LyricsDocument(lines: [wordTimed, following], instrumental: false, precision: "word")
        let states: [(LyricsLine, Double, Bool)] = [
            (timed, 5000, false), (timed, 3000, true), (timed, 1000, false),
            (timed, 3000, false), (plain, 0, false), (plain, 0, false),
            (wordTimed, 3800, wordDocument.focusedLine(at: 3800) == 0),
            (wordTimed, 4999, wordDocument.focusedLine(at: 4999) == 0),
            (wordTimed, 5000, wordDocument.focusedLine(at: 5000) == 0),
            (following, 5000, wordDocument.focusedLine(at: 5000) == 1)
        ]
        let rowHeight: CGFloat = 48, inset: CGFloat = 16
        let appearanceSize = NSSize(width: 460, height: rowHeight * CGFloat(states.count) + inset * 2)
        try await render(VStack(alignment: .leading, spacing: 0) {
            ForEach(states.indices, id: \.self) { index in
                let (line, milliseconds, focused) = states[index]
                LyricsVocalGroupView(line: line, milliseconds: milliseconds, timed: line.startMs != nil,
                                     reduceMotion: true, focused: focused)
                    .frame(height: rowHeight, alignment: .center)
            }
        }
        .padding(inset).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(.black), name: "lyrics-active-plain", size: appearanceSize)
        let png = try Data(contentsOf: evidence.appendingPathComponent("lyrics-active-plain.png"))
        guard let appearance = NSBitmapImageRep(data: png) else {
            throw Failure(message: "Lyric contrast raster unavailable")
        }
        let scale = CGFloat(appearance.pixelsHigh) / appearanceSize.height
        var brightInk: [Int] = []
        for index in states.indices {
            let top = Int((inset + CGFloat(index) * rowHeight) * scale)
            let bottom = Int((inset + CGFloat(index + 1) * rowHeight) * scale)
            var count = 0
            for y in top..<bottom {
                for x in 0..<(appearance.pixelsWide / 2) {
                    if let color = appearance.colorAt(x: x, y: y)?.usingColorSpace(.sRGB),
                       min(color.redComponent, color.greenComponent, color.blueComponent) > 0.82 {
                        count += 1
                    }
                }
            }
            brightInk.append(count)
        }
        try check(brightInk[1] > 100 && brightInk[0] < brightInk[1] / 10 && brightInk[2] < brightInk[1] / 10,
                  "Only the active timed lyric keeps bright ink in the native raster")
        try check(brightInk[3] < brightInk[1] / 10, "An overlapping lyric outside the focused group stays dim")
        try check(brightInk[4] >= brightInk[1] * 3 / 4 && brightInk[5] >= brightInk[1] * 3 / 4,
                  "Every untimed lyric row keeps bright ink in the native raster")
        try check(brightInk[6] >= brightInk[1] * 9 / 10 && brightInk[7] >= brightInk[6] * 9 / 10
                  && brightInk[8] < brightInk[7] / 10 && brightInk[9] > 100,
                  "The full word-timed row stays bright until the next row starts, then the new row takes over")

        let words = [
            LyricsWord(text: "The ", startMs: 1000, endMs: 1600, endEstimated: false),
            LyricsWord(text: "light", startMs: 1600, endMs: 5800, endEstimated: false)
        ]
        let held = LyricsLine(text: "The light", startMs: 1000, endMs: 6000, words: words)
        let renderer = LyricsTextLayout()
        renderer.setLine(held)
        _ = renderer.measure(width: 220)
        guard let heldStart = renderer.words.last?.segments.first?.rect.minX else {
            throw Failure(message: "Missing sustained lyric word geometry")
        }
        let heldFirstPixel = Int(20 + heldStart * 2) - 5
        func drawHeld(at milliseconds: Double, phase: LyricsPaintPhase = .sweeping,
                      reduceMotion: Bool, reduceTransparency: Bool = false) throws -> NSBitmapImageRep {
            guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 480, pixelsHigh: 160,
                                                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
                  let context = NSGraphicsContext(bitmapImageRep: bitmap)?.cgContext else {
                throw Failure(message: "Sustained lyric raster unavailable")
            }
            context.setFillColor(NSColor.black.cgColor)
            context.fill(CGRect(x: 0, y: 0, width: bitmap.pixelsWide, height: bitmap.pixelsHigh))
            context.translateBy(x: 20, y: 125)
            context.scaleBy(x: 2, y: -2)
            NSGraphicsContext.saveGraphicsState()
            defer { NSGraphicsContext.restoreGraphicsState() }
            NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
            renderer.draw(milliseconds: milliseconds, phase: phase, reduceMotion: reduceMotion,
                          reduceTransparency: reduceTransparency)
            return bitmap
        }
        func compareMotion(at milliseconds: Double) throws -> (changedPixels: Int, shiftedCores: Int) {
            let motion = try drawHeld(at: milliseconds, reduceMotion: false)
            let reduced = try drawHeld(at: milliseconds, reduceMotion: true)
            var count = 0, shiftedCores = 0
            for y in 0..<motion.pixelsHigh {
                for x in 0..<motion.pixelsWide {
                    let first = motion.colorAt(x: x, y: y)?.redComponent ?? 0
                    let second = reduced.colorAt(x: x, y: y)?.redComponent ?? 0
                    if abs(first - second) > 0.06 { count += 1 }
                    if x >= heldFirstPixel, max(first, second) > 0.86, min(first, second) < 0.64 {
                        shiftedCores += 1
                    }
                }
            }
            return (count, shiftedCores)
        }
        let before = try compareMotion(at: 1400)
        let during = try compareMotion(at: 3700)
        let after = try compareMotion(at: 5900)
        try check(before.changedPixels == 0 && during.changedPixels > 20 && after.changedPixels == 0,
                  "Native held-word glow appears only inside the real cue and disappears with Reduce Motion")
        try check(during.shiftedCores == 0, "Sustained glow leaves the bright glyph cores in their original positions")
        let glass = try drawHeld(at: 6000, phase: .sung, reduceMotion: true)
        let solid = try drawHeld(at: 6000, phase: .sung, reduceMotion: true, reduceTransparency: true)
        var glassInk = [CGFloat]()
        for y in 0..<solid.pixelsHigh {
            for x in 0..<solid.pixelsWide {
                if let opaque = solid.colorAt(x: x, y: y), opaque.redComponent > 0.995,
                   let color = glass.colorAt(x: x, y: y) {
                    let brightness = (color.redComponent + color.greenComponent + color.blueComponent) / 3
                    if brightness > 0.78 { glassInk.append(brightness) }
                }
            }
        }
        let glassRange = (glassInk.max() ?? 0) - (glassInk.min() ?? 0)
        try check(glassInk.count > 100 && glassRange > 0.015,
                  "Glass varies the fill inside native glyphs while Reduce Transparency uses solid ink")
        try check(glass.colorAt(x: glass.pixelsWide - 24, y: glass.pixelsHigh / 2)?.redComponent == 0,
                  "Glass leaves the space beside the lyrics transparent")
        for (name, raster) in [("lyrics-glass-ink", glass), ("lyrics-reduced-transparency", solid)] {
            guard let png = raster.representation(using: .png, properties: [:]) else {
                throw Failure(message: "Lyric glass PNG unavailable")
            }
            try png.write(to: evidence.appendingPathComponent(name + ".png"))
        }
        try await render(VStack(alignment: .leading, spacing: 20) {
            LyricsLineView(line: held, milliseconds: 3700, active: true, timed: true, reduceMotion: false)
                .frame(width: 320, alignment: .leading)
            LyricsLineView(line: held, milliseconds: 3700, active: true, timed: true, reduceMotion: true)
                .frame(width: 320, alignment: .leading)
        }
        .padding(32).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(.black), name: "lyrics-held-word-motion", size: NSSize(width: 540, height: 260))
    }

    func lyricsUnavailableChecks(_ item: MediaItem) async throws {
        let provider = FailingLyrics(.missing, delay: .milliseconds(1100))
        let player = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            FixtureConnection(receive, items: [item])
        }, lyricsProvider: provider)
        let lyrics = player.lyrics
        let size = NSSize(width: 500, height: 320)
        let view = LyricsView(player: player, lyrics: lyrics).environment(\.colorScheme, .dark)
            .frame(width: size.width, height: size.height).background(.black)
        lyrics.select(item)
        lyrics.setVisible(true)
        try await render(view, name: "lyrics-finding-centered", size: size, ready: { lyrics.isLoading })
        try await render(view, name: "lyrics-unavailable-centered", size: size, ready: { lyrics.unavailability != nil })

        for (name, row) in [("lyrics-finding-centered", 0), ("lyrics-unavailable-centered", 1)] {
            let png = try Data(contentsOf: evidence.appendingPathComponent(name + ".png"))
            guard let raster = NSBitmapImageRep(data: png) else { throw Failure(message: "Missing centered lyric placeholder") }
            var bounds = CGRect.null
            for y in 0..<raster.pixelsHigh {
                for x in 0..<raster.pixelsWide {
                    if let color = raster.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), color.redComponent > 0.28 {
                        bounds = bounds.union(CGRect(x: x, y: y, width: 1, height: 1))
                    }
                }
            }
            try check(!bounds.isNull && abs(bounds.midX / CGFloat(raster.pixelsWide) - 0.5) < 0.06
                      && abs(bounds.midY / CGFloat(raster.pixelsHigh) - 0.5) < 0.06,
                      "Lyrics placeholder \(row) is centered on both axes")
        }

        let host = NSHostingView(rootView: view)
        let window = SnapshotWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: .titled, backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFront(nil)
        defer { window.close() }
        for attempt in 1...2 {
            if attempt == 2 { lyrics.setVisible(true) }
            try await wait { lyrics.unavailability != nil }
            try await Task.sleep(for: .milliseconds(1050))
            try check(lyrics.isVisible, "Lyrics message remains visible before its auto-close deadline")
            try await wait { !lyrics.isVisible }
        }
        try check(await provider.attempts == 2, "Reopening missing lyrics repeats lookup and timed dismissal")
        await player.prepareToQuit()
    }
    func spokenCatalogChecks(_ item: MediaItem) async throws {
        let bookID = String(repeating: "b", count: 22)
        let unavailable = MediaCapabilities(play: false, pause: true, seek: true, volume: true,
                                            pcmAnalysis: false, systemMediaSession: false, librarySave: false)
        let chapter = MediaItem(id: "spotify:chapter:" + String(repeating: "c", count: 22),
            title: "Original chapter · The quiet observatory", artists: [], album: nil,
            durationMs: 120000, artwork: nil, capabilities: unavailable)
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [chapter], details: .manual); driver = connection; return connection
        })
        let connection = driver!
        connection.receive(.remoteState(frame(item, playing: false)))
        let book = RemoteCollection(id: bookID, title: "Original book", url: "https://open.spotify.com/audiobook/" + bookID,
                                    artwork: nil, kind: .audiobook)
        model.querySpotify(kind: .audiobookChapters, text: bookID)
        try await wait { model.spotifyPage?.queryId == model.spotifyQuery.queryId }
        model.inspectSpotify(.audiobook, text: bookID)
        let request = model.spotifyDetailQueries.values.first { $0.kind == .audiobook && $0.text == bookID }!
        let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 2, pixelsHigh: 2, bitsPerSample: 8,
                                      samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        for offset in stride(from: 0, to: bitmap.bytesPerRow * bitmap.pixelsHigh, by: 4) {
            bitmap.bitmapData![offset] = 20; bitmap.bitmapData![offset + 1] = 115
            bitmap.bitmapData![offset + 2] = 107; bitmap.bitmapData![offset + 3] = 255
        }
        let cover = evidence.appendingPathComponent("original-book-cover.png")
        try bitmap.representation(using: .png, properties: [:])!.write(to: cover)
        let decodedCover = await LocalArtworkLoader.shared.image(for: cover.path)
        try check(decodedCover != nil, "The original book cover decodes before native catalog rendering")
        let fresh = RemoteCollection(id: bookID, title: "Original book · Stories of the observatory", url: book.url,
            artwork: cover.path, kind: .audiobook, subtitle: "Original author, Второй автор")
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: request.queryId,
            offset: 0, total: 1, items: [], collections: [fresh], issue: nil)))
        let count = connection.recorded().count
        let projected = model.spotifyCatalogItem(chapter, in: book)
        try check(projected.artist == fresh.subtitle && projected.album == fresh.title && projected.artwork == fresh.artwork,
                  "Simplified chapters reuse the matching book's fresh author, title and cover")
        try check(projected.id == chapter.id && projected.durationMs == chapter.durationMs
                  && !projected.capabilities.play && projected.capabilities.librarySave == false
                  && !projected.capabilities.pcmAnalysis && !projected.capabilities.systemMediaSession,
                  "Parent metadata cannot make an unavailable chapter playable or claim audio ownership")
        try check(connection.recorded().count == count && model.spotifyPage?.items.first?.artists.isEmpty == true,
                  "Enriching a catalog row performs no extra requests and leaves the provider page intact")
        let explicit = MediaItem(id: chapter.id, title: chapter.title, artists: ["Explicit chapter author"], album: "Explicit edition",
            durationMs: chapter.durationMs, artwork: "file:///original-explicit-cover.png", capabilities: chapter.capabilities)
        let preserved = model.spotifyCatalogItem(explicit, in: book)
        try check(preserved.artists == explicit.artists && preserved.album == explicit.album && preserved.artwork == explicit.artwork,
                  "Parent metadata fills omissions without overwriting explicit child metadata")
        let wrongBook = RemoteCollection(id: String(repeating: "d", count: 22), title: "Other book", url: book.url,
                                         artwork: nil, kind: .audiobook, subtitle: "Other author")
        try check(model.spotifyCatalogItem(chapter, in: wrongBook).artists.isEmpty,
                  "A chapter cannot inherit the creator of a different collection")
        let navigation = AppNavigation(); navigation.openCollection(book)
        try await render(SpotifyCollectionView(player: model, navigation: navigation).background(AuvraColors.background),
                         name: "audiobook-chapters", size: NSSize(width: 960, height: 650),
                         ready: { model.spotifyPage?.queryId == model.spotifyQuery.queryId })
        model.querySpotify(kind: .audiobookChapters, text: wrongBook.id)
        try check(model.spotifyCatalogItem(chapter, in: book).artists.isEmpty
                  && model.spotifyCatalogItem(chapter, in: wrongBook).artists.isEmpty,
                  "A pending collection change cannot mix old rows with a new parent")
        connection.receive(.remoteState(RemoteState(provider: "spotify", connection: .disconnected,
            authenticationRequired: true, issue: nil, retryAfterSeconds: nil, devices: [], playback: nil)))
        try check(model.spotifyCatalogItem(chapter, in: book).artists.isEmpty,
                  "Revoked account metadata cannot enrich retained chapter selections")
        await model.prepareToQuit()
    }
    func spokenProgressChecks(_ item: MediaItem) async throws {
        let episodeID = String(repeating: "e", count: 22)
        let uri = "spotify:episode:" + episodeID
        let episode = MediaItem(id: uri, title: "Original episode with saved progress", artists: ["Original podcast"],
                                album: nil, durationMs: 120_000, artwork: nil, capabilities: capabilities,
                                location: .remote(uri: uri, url: "https://open.spotify.com/episode/" + episodeID, service: "Spotify"))
        let progress = SpokenProgress(mediaId: uri, positionMs: 34_500, fullyPlayed: false)
        let process = FixtureProcess()
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [episode], details: .manual, mainMode: .manual)
            driver = connection; return connection
        }, playbackRuntime: runtime)
        let connection = driver!
        project(connection, frame(item, playing: false))
        let profile = model.spotifyDetailQueries.values.first { $0.kind == .profile }!
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: profile.queryId,
            offset: 0, total: 1, items: [], collections: [RemoteCollection(id: "original-listener", title: "Original listener",
                url: "", artwork: nil, kind: .profile)], issue: nil)))
        func pendingEpisode() -> RemoteQuery? { model.spotifyDetailQueries.values.first { $0.kind == .episode } }
        func supplyEpisode(_ query: RemoteQuery, progress: SpokenProgress?) {
            connection.receive(.remotePage(RemotePage(detail: true, spokenProgress: progress.map { [$0] }, provider: "spotify",
                queryId: query.queryId, offset: 0, total: 1, items: [episode], collections: [], issue: nil)))
        }
        model.querySpotify(kind: .searchEpisodes, text: "original")
        let search = model.spotifyQuery.queryId
        connection.receive(.remotePage(RemotePage(spokenProgress: [progress], provider: "spotify", queryId: search,
                                                  offset: 0, total: 1, items: [episode], collections: [], issue: nil)))
        try check(model.spotifySpokenProgress(for: episode, queryId: search)?.continuePosition == 34_500
                  && model.current?.id == item.id && !model.isPlaying,
                  "A catalog bookmark is visible without changing or starting the selected track")
        project(connection, frame(episode, playing: false, position: 65_000), restored: RestoredRemote(contextUri: nil))
        try check(model.spotifySpokenProgress(for: episode, queryId: search)?.continuePosition == 34_500
                  && model.snapshot?.restoredRemote != nil && !model.isPlaying,
                  "A paused session restore does not invalidate an independent catalog bookmark")
        let restoredDetail = pendingEpisode()!
        supplyEpisode(restoredDetail, progress: progress)
        project(connection, frame(item, playing: false))
        let navigation = AppNavigation()
        var inspectorQueryID: UInt64?
        var hiddenDuringRefresh = false
        try await render(SpotifyItemInspector(player: model, item: episode, navigation: navigation).background(AuvraColors.background),
                         name: "light-spoken-progress", size: NSSize(width: 580, height: 540), colorScheme: .light, ready: {
            if let query = pendingEpisode() {
                hiddenDuringRefresh = model.spotifySpokenProgress(for: episode, queryId: restoredDetail.queryId) == nil
                inspectorQueryID = query.queryId
                supplyEpisode(query, progress: progress)
            }
            return inspectorQueryID.map { model.spotifySpokenProgress(for: episode, queryId: $0)?.continuePosition == 34_500 } == true
        })
        try check(inspectorQueryID! > restoredDetail.queryId && hiddenDuringRefresh,
                  "Opening the inspector refreshes a cached episode and hides its old Continue while the lookup is pending")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let inFlight = pendingEpisode()!
        model.continueSpotifySpoken(episode, queryId: search)
        let start = connection.recorded().compactMap { command -> RemoteControl? in
            if case .remote(_, .control(let control)) = command, case .startSpokenOnDevice = control { return control }
            return nil
        }.last
        guard let start, case .startSpokenOnDevice(let selected, let device, let position, let operation) = start else {
            throw Failure(message: "Explicit spoken continuation did not send a positioned start")
        }
        try check(selected.id == uri && device == "owned-output" && position == 34_500 && operation > 0,
                  "Continue targets the fresh catalog bookmark and owned device as a new operation")
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let wire = try JSONSerialization.jsonObject(with: encoder.encode(start)) as! [String: Any]
        try check(wire["kind"] as? String == "start_spoken_on_device" && wire["position_ms"] as? UInt64 == 34_500
                  && wire["operation_id"] as? UInt64 == operation && wire["context_uri"] == nil,
                  "Spoken Continue encodes a targeted position without inventing a restored context")
        project(connection, frame(episode, position: 0))
        try check(model.isLoading, "A matching episode at the wrong position does not finish Continue")
        project(connection, frame(episode, position: 34_800))
        try check(!model.isLoading && model.current?.id == uri && model.isPlaying,
                  "Actual playback near the requested bookmark confirms Continue")
        supplyEpisode(inFlight, progress: SpokenProgress(mediaId: uri, positionMs: 56_000, fullyPlayed: false))
        try check(model.spotifySpokenProgress(for: episode, queryId: search) == nil
                  && model.spotifySpokenProgress(for: episode, queryId: inFlight.queryId) == nil,
                  "Active playback hides Continue from both the catalog and an in-flight inspector response")
        project(connection, frame(episode, playing: false, position: 55_000))
        let afterPause = connection.recorded().count
        model.continueSpotifySpoken(episode, queryId: inFlight.queryId)
        try check(model.spotifySpokenProgress(for: episode, queryId: inFlight.queryId) == nil
                  && connection.recorded().count == afterPause,
                  "A read started before playback advanced cannot offer a positioned restart after pause")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let lagging = pendingEpisode()!
        supplyEpisode(lagging, progress: progress)
        try check(model.spotifySpokenProgress(for: episode, queryId: lagging.queryId) == nil,
                  "Even a new provider response cannot rewind a just-played episode while its bookmark is catching up")
        let chapterURI = "spotify:chapter:" + episodeID
        let chapter = MediaItem(id: chapterURI, title: episode.title, artists: episode.artists, album: nil, durationMs: episode.durationMs,
                                artwork: nil, capabilities: capabilities, location: .remote(uri: chapterURI, url: "https://open.spotify.com/chapter/" + episodeID, service: "Spotify"))
        model.inspectSpotify(.chapter, text: episodeID)
        let chapterQuery = model.spotifyDetailQueries.values.first { $0.kind == .chapter }!
        connection.receive(.remotePage(RemotePage(detail: true,
            spokenProgress: [SpokenProgress(mediaId: chapterURI, positionMs: progress.positionMs, fullyPlayed: false)], provider: "spotify",
            queryId: chapterQuery.queryId, offset: 0, total: 1, items: [chapter], collections: [], issue: nil)))
        try check(model.spotifySpokenProgress(for: chapter, queryId: chapterQuery.queryId) == nil,
                  "An audiobook chapter cannot rewind playback observed through its episode alias")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let caughtUp = pendingEpisode()!
        supplyEpisode(caughtUp, progress: SpokenProgress(mediaId: uri, positionMs: 55_000, fullyPlayed: false))
        try check(model.spotifySpokenProgress(for: episode, queryId: caughtUp.queryId)?.continuePosition == 55_000,
                  "A new caught-up bookmark is eligible after playback pauses")

        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let beforeRewind = pendingEpisode()!
        model.seek(milliseconds: 28_000)
        project(connection, frame(episode, playing: false, position: 28_000))
        supplyEpisode(beforeRewind, progress: SpokenProgress(mediaId: uri, positionMs: 28_000, fullyPlayed: false))
        try check(model.spotifySpokenProgress(for: episode, queryId: beforeRewind.queryId) == nil,
                  "A paused seek fences even a matching bookmark from a query started before the seek")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let beforeBookmarkCatchesUp = pendingEpisode()!
        supplyEpisode(beforeBookmarkCatchesUp, progress: SpokenProgress(mediaId: uri, positionMs: 55_000, fullyPlayed: false))
        try check(model.spotifySpokenProgress(for: episode, queryId: beforeBookmarkCatchesUp.queryId) == nil,
                  "A paused rewind hides the earlier bookmark even in a new response")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let afterRewind = pendingEpisode()!
        supplyEpisode(afterRewind, progress: SpokenProgress(mediaId: uri, positionMs: 27_999, fullyPlayed: false))
        try check(model.spotifySpokenProgress(for: episode, queryId: afterRewind.queryId)?.continuePosition == 27_999 && !model.isPlaying,
                  "A fresh bookmark near the new paused position offers Continue after a backward seek")

        model.querySpotify(kind: .searchEpisodes, text: "new query")
        let newQuery = model.spotifyQuery.queryId
        let beyondEnd = SpokenProgress(mediaId: uri, positionMs: 120_001, fullyPlayed: false)
        connection.receive(.remotePage(RemotePage(spokenProgress: [beyondEnd], provider: "spotify", queryId: newQuery,
                                                  offset: 0, total: 1, items: [episode], collections: [], issue: nil)))
        let count = connection.recorded().count
        model.continueSpotifySpoken(episode, queryId: search)
        model.continueSpotifySpoken(episode, queryId: newQuery)
        try check(connection.recorded().count == count, "A stale query and out-of-range progress cannot start a different position")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let legacy = pendingEpisode()!
        supplyEpisode(legacy, progress: nil)
        try check(model.spotifySpokenProgress(for: episode, queryId: legacy.queryId) == nil,
                  "A legacy episode detail without a progress sidecar offers no continuation")
        model.inspectSpotify(.episode, text: episodeID, refresh: true)
        let completion = pendingEpisode()!
        let played = SpokenProgress(mediaId: uri, positionMs: 120_000, fullyPlayed: true)
        supplyEpisode(completion, progress: played)
        let completed = model.spotifySpokenProgress(for: episode, queryId: completion.queryId)
        try check(completed?.fullyPlayed == true && completed?.continuePosition == nil,
                  "A completed episode is marked played without offering end-of-file continuation")
        connection.receive(.remoteState(RemoteState(provider: "spotify", connection: .disconnected,
            authenticationRequired: true, issue: nil, retryAfterSeconds: nil, devices: [], playback: nil)))
        try check(model.spotifySpokenProgress(for: episode, queryId: completion.queryId) == nil,
                  "Revoking Spotify removes saved listening progress from open views")
        await model.prepareToQuit()
    }
    func linkedCatalogChecks(_ item: MediaItem) async throws {
        let id = String(repeating: "aB", count: 11)
        let kinds: [(String, RemoteCatalogKind)] = [
            ("track", .track), ("album", .album), ("artist", .artist), ("playlist", .playlistInfo),
            ("show", .show), ("episode", .episode), ("audiobook", .audiobook), ("chapter", .chapter)
        ]
        for (type, kind) in kinds {
            try check(SpotifySearchInput(" Spotify:\(type):\(id) ") == .reference(kind, id)
                      && SpotifySearchInput("https://open.spotify.com/intl-de/\(type)/\(id)?si=original#ignored") == .reference(kind, id),
                      "URI and localized share link both resolve the exact \(type)")
        }
        try check(SpotifySearchInput("http://open.spotify.com/track/" + id) == .reference(.track, id)
                  && SpotifySearchInput("open.spotify.com/show/" + id) == .reference(.show, id)
                  && SpotifySearchInput(" Aurora ") == .keywords("Aurora"), "Copied links and ordinary search terms are distinguished")
        let profileID = "Original.Listener_42-demo", profileURI = "spotify:user:Original.Listener_42-demo"
        try check(SpotifySearchInput(" Spotify:user:" + profileID + " ") == .userProfile(profileID)
                  && SpotifySearchInput("https://open.spotify.com/intl-de/user/" + profileID + "/?si=original#ignored") == .userProfile(profileID)
                  && SpotifySearchInput("open.spotify.com/user/" + profileID) == .userProfile(profileID),
                  "Exact Spotify profile links preserve bounded legacy user IDs without treating them as catalog entities")
        for invalid in ["https://open.spotify.com.example/track/" + id, "https://name@open.spotify.com/track/" + id,
                        "https://open.spotify.com:444/track/" + id, "https://spotify.link/original",
                        "spotify:user:..", "spotify:user:a,b", "spotify:user:a:b", "spotify:user:" + String(repeating: "a", count: 129),
                        "https://open.spotify.com/user/a%2Fb", "https://open.spotify.com/user//alice", "https://open.spotify.com/user/../",
                        "https://open.spotify.com.example/user/alice", "spotify:episode:" + id + ":extra", "spotify:track:short"] {
            try check(SpotifySearchInput(invalid) == .unsupportedLink, "An unsupported link cannot become a catalog query")
        }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [self.track(title: "Original episode · How sound travels")]); driver = connection; return connection
        })
        let connection = driver!
        model.querySpotify(kind: .search, text: "Original retained query", offset: 10)
        try await wait { model.spotifyPage?.queryId == model.spotifyQuery.queryId }
        let catalogID = model.spotifyQuery.queryId
        connection.receive(.remoteState(frame(item, playing: false)))
        model.inspectSpotify(.episode, text: id)
        try await wait { model.spotifyDetail(.episode, text: id) != nil }
        let cachedEpisodeQuery = model.spotifyDetail(.episode, text: id)!.queryId
        let navigation = AppNavigation()
        navigation.searchText = "https://open.spotify.com/episode/" + id + "?si=original"
        try await render(SearchView(player: model, navigation: navigation).background(AuvraColors.background)
            .transaction { $0.disablesAnimations = true },
                         name: "light-linked-episode", size: NSSize(width: 840, height: 470), colorScheme: .light,
                         ready: {
            guard let episode = model.spotifyDetail(.episode, text: id) else { return false }
            return episode.queryId > cachedEpisodeQuery && episode.items.first?.id == "spotify:episode:" + id
        })
        try check(model.spotifyPage?.queryId == catalogID && model.spotifyQuery.offset == 10
                  && !connection.recorded().contains { if case .remote(_, .control) = $0 { return true }; return false },
                  "Link lookup preserves the main page and cannot autoplay")
        let before = connection.recorded().count
        navigation.searchText = "https://open.spotify.com.example/track/" + id
        try await render(SearchView(player: model, navigation: navigation).background(AuvraColors.background),
                         name: "invalid-content-link", size: NSSize(width: 840, height: 470))
        try check(connection.recorded().count == before, "An unsupported URL leaves both catalogs untouched")
        navigation.searchText = profileURI
        try await render(SearchView(player: model, navigation: navigation).background(AuvraColors.background),
                         name: "light-linked-profile", size: NSSize(width: 840, height: 470), colorScheme: .light)
        try check(connection.recorded().count == before && model.spotifySaved[profileURI] == nil,
                  "Opening a pasted profile shows unknown follow status without querying a profile or changing the account")

        model.checkSpotifyURI(profileURI)
        guard case .remote(_, .edit(let checkFollow)) = connection.recorded().last else { throw Failure(message: "Missing profile follow status request") }
        try check(checkFollow.edit.kind == "check_library" && checkFollow.edit.uris == [profileURI] && model.spotifySaved[profileURI] == nil,
                  "Check follow status uses the exact profile URI and awaits the library result")
        connection.receive(.remoteEdited(.init(operationId: checkFollow.operationId, collection: nil, error: nil, saved: [.init(uri: profileURI, saved: false)])))
        model.saveSpotifyURI(profileURI, save: true)
        guard case .remote(_, .edit(let follow)) = connection.recorded().last else { throw Failure(message: "Missing explicit Follow request") }
        try check(follow.edit.kind == "library" && follow.edit.uris == [profileURI] && follow.edit.save == true && model.spotifySaved[profileURI] == false,
                  "Explicit Follow targets that profile without optimistic membership")
        connection.receive(.remoteEdited(.init(operationId: follow.operationId, collection: nil, error: nil, saved: [])))
        model.saveSpotifyURI(profileURI, save: false)
        guard case .remote(_, .edit(let unfollow)) = connection.recorded().last else { throw Failure(message: "Missing explicit Unfollow request") }
        try check(unfollow.edit.kind == "library" && unfollow.edit.uris == [profileURI] && unfollow.edit.save == false && model.spotifySaved[profileURI] == true,
                  "Unfollow is a separate explicit operation and preserves confirmed membership while pending")
        connection.receive(.remoteEdited(.init(operationId: unfollow.operationId, collection: nil, error: nil, saved: [])))
        try check(model.spotifySaved[profileURI] == false && !model.spotifyEditPending, "The follow indicator changes only after confirmed Unfollow")
        await model.prepareToQuit()
    }
    func mixedSearchRankingChecks() throws {
        func collection(_ kind: RemoteEntityKind, _ id: String, _ title: String) -> RemoteCollection {
            let id = String(repeating: id, count: 22)
            return RemoteCollection(id: id, title: title, url: "https://open.spotify.com/\(kind.rawValue)/" + id, artwork: nil, kind: kind)
        }
        func page(items: [MediaItem] = [], collections: [RemoteCollection]) -> RemotePage {
            RemotePage(provider: "spotify", queryId: 1, offset: 0, total: 4, items: items, collections: collections, issue: nil)
        }

        let exact = collection(.artist, "a", "Beyoncé")
        let near = collection(.artist, "b", "Beyoncc")
        let ranked = MixedSpotifyResults(page: page(collections: [near, exact]), query: "  BEYONCE!  ")
        try check(ranked.artist?.uri == exact.uri && ranked.more.map(\.title) == [near.title],
                  "Mixed search leads with the normalized exact artist once even when the provider lists a typo first")
        let typo = MixedSpotifyResults(page: page(collections: [near, exact]), query: "Beynocé")
        try check(typo.artist?.uri == exact.uri, "Mixed search promotes a unique adjacent-letter typo")
        let aurora = collection(.artist, "c", "Aurora")
        let aurori = collection(.artist, "d", "Aurori")
        let ambiguous = MixedSpotifyResults(page: page(collections: [aurora, aurori]), query: "Auroro")
        try check(ambiguous.artist == nil && ambiguous.more.map(\.title) == [aurora.title, aurori.title],
                  "Two similarly close artists stay in ordinary results in the provider's order")

        let firstSong = track("f", title: "First song")
        let exactSong = track("g", title: "Original")
        let lastSong = track("h", title: "Last song")
        let episode = MediaItem(id: "spotify:episode:" + String(repeating: "e", count: 22),
                                title: "Original episode", artists: ["Original creator"], album: nil,
                                durationMs: 120000, artwork: nil, capabilities: capabilities)
        let firstAlbum = collection(.album, "i", "First album")
        let exactAlbum = collection(.album, "j", "Original")
        let podcast = collection(.show, "k", "Original podcast")
        let result = MixedSpotifyResults(page: page(items: [firstSong, episode, exactSong, lastSong],
                                                     collections: [firstAlbum, aurora, exactAlbum, podcast]), query: "Original")
        let hitIDs = result.music.map { hit in
            switch hit {
            case .item(_, let item): item.id
            case .collection(let collection): collection.uri
            }
        }
        let songOffsets = result.music.compactMap { hit -> Int? in
            if case .item(let offset, _) = hit { return offset }
            return nil
        }
        try check(result.artist == nil && hitIDs == [exactSong.id, exactAlbum.uri, firstSong.id, firstAlbum.uri, lastSong.id],
                  "Mixed search boosts exact music titles and interleaves the remaining songs and albums in provider order")
        try check(songOffsets == [2, 0, 3] && result.more.map(\.title) == [aurora.title, podcast.title, episode.title],
                  "Mixed ranking preserves row identities and keeps non-music results available")
        let auroraSong = MediaItem(id: "spotify:track:" + String(repeating: "l", count: 22),
                                   title: "Runaway", artists: ["AURORA"], album: nil,
                                   durationMs: 240000, artwork: nil, capabilities: capabilities)
        let auroraAlbum = RemoteCollection(id: String(repeating: "m", count: 22), title: "All My Demons",
                                           url: "https://open.spotify.com/album/", artwork: nil, kind: .album, subtitle: "Aurora")
        let sameTitle = collection(.album, "n", "Aurora")
        let artistResults = MixedSpotifyResults(page: page(items: [firstSong, auroraSong],
            collections: [aurora, sameTitle, auroraAlbum]), query: "AURORA")
        try check(artistResults.artist?.uri == aurora.uri && artistResults.music.map(\.title) == ["Runaway", "All My Demons", "Aurora", "First song"],
                  "Music by the leading artist appears before unrelated releases named after the search")
    }
    func detailChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item],details: .manual); driver = connection; return connection
        })
        let connection = driver!
        model.querySpotify(kind: .search,text: "original")
        try await wait { model.spotifyPage != nil }
        let catalogID = model.spotifyQuery.queryId
        let albumID = String(repeating: "a",count: 22)
        model.inspectSpotify(.profile); model.inspectSpotify(.album,text: albumID)
        try check(model.spotifyDetailQueries.count == 2 && model.spotifyQuery.queryId == catalogID, "Profile and album requests preserve the visible catalog intent")
        let profile = model.spotifyDetailQueries.values.first { $0.kind == .profile }!
        let album = model.spotifyDetailQueries.values.first { $0.kind == .album }!
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify",queryId: album.queryId,offset: 0,total: 1,items: [],collections: [RemoteCollection(id: albumID,title: "Fresh original album",url: "https://open.spotify.com/album/" + albumID,artwork: nil,kind: .album)],issue: nil)))
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify",queryId: profile.queryId,offset: 0,total: 1,items: [],collections: [RemoteCollection(id: "listener",title: "Original listener",url: "https://open.spotify.com/",artwork: nil,kind: .profile)],issue: nil)))
        try check(model.spotifyDetail(.profile)?.collections.first?.title == "Original listener" && model.spotifyDetail(.album,text: albumID)?.collections.first?.title == "Fresh original album" && model.spotifyPage?.queryId == catalogID, "Concurrent detail replies are routed by query ID without changing the catalog page")
        model.inspectSpotify(.album,text: albumID)
        try check(model.spotifyDetailQueries.isEmpty, "Cached details do not enqueue repeated reads")
        model.inspectSpotify(.album,text: albumID,refresh: true)
        let reload = model.spotifyDetailQueries.values.first!
        connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: album.queryId,offset: 0,total: 0,items: [],collections: [],issue: nil)))
        try check(model.spotifyDetailQueries[reload.queryId] != nil && model.spotifyDetail(.album,text: albumID)?.collections.first?.title == "Fresh original album", "An older detail completion cannot finish a refresh or erase fresh metadata")
        connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: reload.queryId,offset: 0,total: 0,items: [],collections: [],issue: PlayerIssue(code: "permissions",message: "Original detail failure"))))
        try check(!model.isLoadingSpotifyDetail(.album,text: albumID) && model.spotifyDetail(.album,text: albumID)?.issue?.message == "Original detail failure", "Detail errors finish loading and remain retryable independently of the catalog")
        model.inspectSpotify(.playlistImages,text: albumID)
        let coverRequest = model.spotifyDetailQueries.values.first!
        let pinned = RemoteCollection(id: albumID,title: "Original playlist",url: "https://open.spotify.com/playlist/" + albumID,artwork: "https://i.scdn.co/image/original-old-cover",kind: .playlist)
        connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: coverRequest.queryId,offset: 0,total: 1,items: [],collections: [RemoteCollection(id: albumID,title: "Playlist cover",url: pinned.url,artwork: nil,kind: .playlist)],issue: nil)))
        try check(model.collectionDetails(pinned).artwork == nil, "A successful empty cover response removes stale artwork instead of retaining an old image")
        model.inspectSpotify(.track,text: albumID)
        let detailRequest = model.spotifyDetailQueries.values.first!
        let relatedAlbum = RemoteCollection(id: String(repeating: "b",count: 22),title: "Original related album",url: "https://open.spotify.com/album/",artwork: nil,kind: .album)
        let relatedArtist = RemoteCollection(id: String(repeating: "c",count: 22),title: "Original related artist",url: "https://open.spotify.com/artist/",artwork: nil,kind: .artist)
        let unrelated = RemoteCollection(id: String(repeating: "p",count: 22),title: "Unrelated playlist",url: "https://open.spotify.com/playlist/",artwork: nil,kind: .playlist)
        connection.receive(.remotePage(RemotePage(relatedCollections: [relatedAlbum,unrelated,relatedArtist,relatedArtist],detail: true,provider: "spotify",queryId: detailRequest.queryId,offset: 0,total: 1,items: [item],collections: [],issue: nil)))
        let related = model.spotifyRelatedCollections(for: item)
        try check(related.map(\.entity) == [.album,.artist] && model.spotifyQuery.queryId == catalogID, "Item details expose unique real album and artist links independently of the main catalog")
        model.inspectSpotify(.episode, text: albumID)
        let wrongType = model.spotifyDetailQueries.values.first!
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: wrongType.queryId,
            offset: 0, total: 1, items: [item], collections: [], issue: nil)))
        try check(model.spotifyDetail(.episode, text: albumID)?.issue?.code == "invalid_response"
                  && model.spotifyDetail(.episode, text: albumID)?.items.isEmpty == true,
                  "A different content type with the same ID cannot populate a direct result")
        try check(model.spotifyRelatedCollections(for: track("d")).isEmpty, "Unrelated item details cannot attach links to the next selected track")
        let relatedNavigation = AppNavigation(); relatedNavigation.searchText = "Original retained search"; relatedNavigation.show(.search)
        relatedNavigation.openCollection(relatedAlbum); relatedNavigation.leaveCollection()
        try check(relatedNavigation.page == .search && relatedNavigation.searchText == "Original retained search", "A related collection returns to the original catalog without changing its search text")
        model.inspectSpotify(.artistAlbums, text: albumID)
        let releases = model.spotifyDetailQueries.values.first { $0.kind == .artistAlbums }!
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: releases.queryId,
            offset: 0, total: 12, items: [], collections: [relatedAlbum], issue: nil)))
        model.loadMoreSpotifyArtistAlbums(albumID, at: 10)
        model.loadMoreSpotifyArtistAlbums(albumID, at: 10)
        try check(model.spotifyDetailQueries.count == 1 && model.spotifyDetail(.artistAlbums, text: albumID)?.collections.count == 1,
                  "Artist continuation coalesces repeated requests and keeps the first releases visible")
        let failed = model.spotifyDetailQueries.values.first!
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: failed.queryId,
            offset: 10, total: 0, items: [], collections: [], issue: PlayerIssue(code: "network_error", message: "Original continuation failure"))))
        try check(model.spotifyNextArtistAlbumOffset(albumID) == 10 && model.spotifyDetail(.artistAlbums, text: albumID)?.collections.count == 1,
                  "A failed continuation keeps releases and the same offset for retry")
        model.loadMoreSpotifyArtistAlbums(albumID, at: 10)
        let retry = model.spotifyDetailQueries.values.first!
        let secondRelease = RemoteCollection(id: albumID, title: "Second original release", url: "https://open.spotify.com/album/" + albumID, artwork: nil, kind: .album)
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: retry.queryId,
            offset: 10, total: 12, items: [], collections: [relatedAlbum, secondRelease], issue: nil)))
        try check(model.spotifyDetail(.artistAlbums, text: albumID)?.collections.map(\.id) == [relatedAlbum.id, secondRelease.id]
                  && model.spotifyNextArtistAlbumOffset(albumID) == nil && model.spotifyQuery.queryId == catalogID,
                  "Artist releases append uniquely in order without replacing the underlying search")
        for number in 1...9 { model.inspectSpotify(.artist,text: String(format: "%022d",number)) }
        try check(model.spotifyDetailQueries.count == 8 && model.spotifyDetails.count <= 32, "The host bounds outstanding detail reads without dropping the catalog request")
        connection.receive(.remoteState(RemoteState(provider: "spotify",connection: .disconnected,authenticationRequired: true,issue: nil,retryAfterSeconds: nil,devices: [],playback: nil)))
        try check(model.spotifyDetailQueries.isEmpty && model.spotifyDetails.isEmpty, "Authorization revocation clears account metadata and pending detail generations")
        connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: profile.queryId,offset: 0,total: 0,items: [],collections: [],issue: nil)))
        try check(model.spotifyDetails.isEmpty, "A revoked profile response cannot restore the previous account")
        connection.receive(.remotePage(RemotePage(provider: "spotify",queryId: catalogID,offset: 0,total: 1,items: [item],collections: [],issue: nil)))
        try check(model.spotifyPage == nil, "An already queued catalog reply cannot restore data after account revocation")
        model.acceptSpotifyAccess("original-test-credential")
        try check(model.spotifyQuery.queryId != catalogID && model.spotifyPage == nil, "A new authorization generation reissues the catalog instead of showing previous account data")
        try await wait { model.spotifyPage?.queryId == model.spotifyQuery.queryId }
        try check(model.spotifyPage?.items.first?.id == item.id, "Fresh authorization results reach the catalog through the new query ID")
        await model.prepareToQuit()
        let rejected = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in FixtureConnection(receive,items: [item],details: .reject) })
        rejected.inspectSpotify(.profile)
        try await wait { rejected.spotifyDetail(.profile)?.issue != nil }
        try check(!rejected.isLoadingSpotifyDetail(.profile) && rejected.errorMessage == nil && !rejected.bridgeFailed, "Rejected detail admission produces an error instead of an endless spinner")
        await rejected.prepareToQuit()
        let a = "spotify:track:" + String(repeating: "a",count: 22)
        let b = "https://open.spotify.com/episode/" + String(repeating: "b",count: 22) + "?si=original"
        let draft = PlaylistReplacementDraft(a + "\n" + b + "\n" + a)
        try check(draft.issue == nil && draft.uris.count == 3 && draft.uris[0] == draft.uris[2] && draft.uris[1].hasPrefix("spotify:episode:"), "Replacement preview preserves order and duplicate occurrences while normalizing public links")
        for invalid in ["https://evil.example/track/" + albumID,"https://user:secret@open.spotify.com/track/" + albumID,"spotify:album:" + albumID,String(repeating: a + "\n",count: 101)] { try check(PlaylistReplacementDraft(invalid).issue != nil, "Invalid or oversized playlist replacement is rejected") }
        let page = RemotePage(itemPositions: [10,12],provider: "spotify",queryId: 1,offset: 10,total: 13,items: [item,item],collections: [],issue: PlayerIssue(code: "unavailable",message: "Omitted item"))
        try check(page.position(at: 1) == 12 && page.position(at: 2) == nil, "Playlist editing uses the source position after unavailable rows and preserves duplicates")
        let old = RemotePage(provider: "spotify",queryId: 1,offset: 10,total: 13,items: [item,item],collections: [],issue: PlayerIssue(code: "unavailable",message: "Omitted item"))
        try check(old.position(at: 1) == nil, "Legacy pages with omitted rows do not guess reorder positions")
    }
    func playlistMetadataChecks(_ item: MediaItem) async throws {
        let id = String(repeating: "p", count: 22)
        let baseline = RemoteCollection(id: id, title: "Original playlist", url: "https://open.spotify.com/playlist/" + id,
                                        artwork: nil, isPublic: true, kind: .playlist, description: "Original description",
                                        ownerId: "original-listener", collaborative: false)
        let rename = RemoteEdit.playlistDetailsChanges(baseline, name: "Renamed only", description: "Original description", published: true)!
        try check(rename.name == "Renamed only" && rename.description == nil && rename.public == nil,
                  "A playlist rename does not submit unchanged description or publishing metadata")
        let clear = RemoteEdit.playlistDetailsChanges(baseline, name: baseline.title, description: "", published: true)!
        try check(clear.description == "" && clear.name == nil && clear.public == nil, "Clearing a playlist description remains an explicit empty-string patch")
        let unpublish = RemoteEdit.playlistDetailsChanges(baseline, name: baseline.title, description: "Original description", published: false)!
        try check(unpublish.public == false && unpublish.name == nil && unpublish.description == nil, "Disabling profile publication sends explicit false without rewriting text")
        try check(RemoteEdit.playlistDetailsChanges(baseline, name: baseline.title, description: "Original description", published: true) == nil,
                  "Opening or reverting an unchanged playlist form cannot produce a save operation")
        let unknown = RemoteCollection(id: id, title: baseline.title, url: baseline.url, artwork: nil)
        let unknownRename = RemoteEdit.playlistDetailsChanges(unknown, name: "Renamed only", description: "", published: false)!
        try check(unknownRename.description == nil && unknownRename.public == nil, "Unknown original metadata is not overwritten with form defaults")
        let enable = RemoteEdit.playlistDetailsChanges(baseline, name: baseline.title, description: "Original description", published: false, collaborating: true)!
        try check(enable.collaborative == true && enable.public == false && enable.name == nil && enable.description == nil,
                  "Enabling collaboration commits only its explicit non-public settings together")
        let collaborative = RemoteCollection(id: id, title: baseline.title, url: baseline.url, artwork: nil, isPublic: false, kind: .playlist,
                                             description: "Original description", ownerId: baseline.ownerId, collaborative: true)
        let disabled = RemoteEdit.playlistDetailsChanges(collaborative, name: collaborative.title, description: "Original description", published: false, collaborating: false)!
        try check(disabled.collaborative == false && disabled.public == nil && disabled.name == nil && disabled.description == nil,
                  "Disabling collaboration leaves profile publication and text untouched")
        let publish = RemoteEdit.playlistDetailsChanges(collaborative, name: collaborative.title, description: "Original description", published: true, collaborating: false)!
        try check(publish.public == true && publish.collaborative == false, "Publishing after collaboration is disabled commits both compatible settings in one operation")
        let collaborativeRename = RemoteEdit.playlistDetailsChanges(collaborative, name: "Renamed only", description: "Original description", published: false, collaborating: true)!
        try check(collaborativeRename.name != nil && collaborativeRename.collaborative == nil && collaborativeRename.public == nil,
                  "Renaming an existing collaborative playlist preserves unrelated access settings")
        try check(RemoteEdit.playlistDetailsChanges(baseline, name: baseline.title, description: "Original description", published: true, collaborating: true) == nil,
                  "A form cannot save simultaneous collaboration and profile publishing")
        let creation = RemoteEdit.createPlaylist(name: "Original collaboration", description: "Original fixture", published: false, collaborating: true)!
        try check(creation.isValidCreatePlaylist && creation.collaborative == true && creation.public == false,
                  "A new collaborative playlist explicitly stays off the profile")
        try check(RemoteEdit.createPlaylist(name: "Original fixture", description: "", published: true, collaborating: true) == nil,
                  "New-playlist validation rejects incompatible publication and collaboration")
        try check(RemoteEdit.playlistDetailsChanges(baseline, name: "Семья 👨‍👩‍👧‍👦 · شب\u{200C}ها", description: "Original description", published: true) != nil,
                  "Playlist names retain emoji ZWJ and language ZWNJ without allowing actual control characters")
        for invalid in [RemoteEdit(kind: "playlist_details", id: id), RemoteEdit(kind: "playlist_details", id: id, name: "  "),
                        RemoteEdit(kind: "playlist_details", id: id, name: "bad\nname"), RemoteEdit(kind: "playlist_details", id: id, name: String(repeating: "я", count: 129)),
                        RemoteEdit(kind: "playlist_details", id: id, description: String(repeating: "я", count: 1025)),
                        RemoteEdit(kind: "playlist_details", id: "bad-id", public: false),
                        RemoteEdit(kind: "playlist_details", id: id, public: true, collaborative: true),
                        RemoteEdit(kind: "playlist_details", id: id, collaborative: true)] {
            try check(!invalid.isValidPlaylistDetailsPatch, "Empty, malformed or oversized metadata patches are rejected before provider admission")
        }
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let body = try JSONSerialization.jsonObject(with: encoder.encode(rename)) as! [String: Any]
        try check(Set(body.keys) == ["kind", "id", "name"], "Swift wire encoding omits untouched metadata rather than sending null or defaults")
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item], details: .manual); driver = connection; return connection
        })
        let connection = driver!
        func reply(_ kind: RemoteCatalogKind, text: String = "", _ collection: RemoteCollection) {
            let query = model.spotifyDetailQueries.values.first { $0.kind == kind && $0.text == text }!
            connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: query.queryId, offset: 0, total: 1, items: [], collections: [collection], issue: nil)))
        }
        model.inspectSpotify(.profile)
        reply(.profile, RemoteCollection(id: "original-listener", title: "Original listener", url: "https://open.spotify.com/", artwork: nil, kind: .profile))
        model.inspectSpotify(.playlistInfo, text: id); reply(.playlistInfo, text: id, baseline)
        model.inspectSpotify(.playlistInfo, text: id, refresh: true)
        let fresh = RemoteCollection(id: id, title: baseline.title, url: baseline.url, artwork: nil, isPublic: false, kind: .playlist,
                                     description: "Updated by another client", ownerId: "original-listener", collaborative: false)
        reply(.playlistInfo, text: id, fresh)
        model.editSpotify(rename)
        let operation = connection.recorded().compactMap { command -> RemoteEditRequest? in if case .remote(_, .edit(let request)) = command { return request }; return nil }.last!
        try check(operation.edit.name == "Renamed only" && operation.edit.description == nil && operation.edit.public == nil
                  && model.collectionDetails(baseline).description == fresh.description,
                  "A details refresh during editing cannot rebase unchanged form fields into a stale overwrite")
        connection.receive(.remoteEdited(RemoteEditResult(operationId: operation.operationId, collection: nil, error: PlayerIssue(code: "network", message: "Original uncertain write fixture"), saved: [])))
        let count = connection.recorded().count
        model.editSpotify(RemoteEdit(kind: "playlist_details", id: id))
        try check(!model.spotifyEditPending && model.spotifyEditIssue != nil && connection.recorded().count == count,
                  "The shared Store rejects an empty metadata patch without a command or retry")
        model.editSpotify(RemoteEdit(kind: "playlist_details", id: id, public: true, collaborative: true))
        model.editSpotify(RemoteEdit(kind: "create_playlist", name: "Original fixture", description: "", public: true, collaborative: true))
        try check(!model.spotifyEditPending && connection.recorded().count == count, "Store admission rejects incompatible collaboration for both creation and editing")
        model.editSpotify(enable)
        let collaborationOperation = connection.recorded().compactMap { command -> RemoteEditRequest? in if case .remote(_, .edit(let request)) = command { return request }; return nil }.last!
        try check(model.spotifyEditPending && collaborationOperation.edit.collaborative == true && collaborationOperation.edit.public == false,
                  "Verified owner access admits the explicit collaboration patch through the shared Store")
        connection.receive(.remoteEdited(RemoteEditResult(operationId: collaborationOperation.operationId, collection: nil, error: PlayerIssue(code: "permissions", message: "Original collaboration permission rejection"), saved: [])))
        try check(!model.spotifyEditPending && model.spotifyEditIssue != nil, "A collaboration permission failure finishes pending without reporting success")
        for (published, collaborating) in [(false, true), (true, false)] {
            let name = published ? "Original published rename" : "Original collaborative rename"
            let original = collaborating ? baseline : collaborative
            let edit = RemoteEdit.playlistDetailsChanges(original, name: name, description: "Original description", published: published, collaborating: collaborating)!
            model.editSpotify(edit)
            let admitted = connection.recorded().compactMap { command -> RemoteEditRequest? in if case .remote(_, .edit(let request)) = command { return request }; return nil }.last!
            connection.receive(.remoteEdited(RemoteEditResult(operationId: admitted.operationId, collection: nil, error: nil, saved: [])))
            try check(model.spotifyCreatedPlaylist?.title == name && model.spotifyCreatedPlaylist?.collaborative == edit.collaborative
                      && model.spotifyCreatedPlaylist?.isPublic == edit.public,
                      "A successful rename projects the explicit collaboration and publication flags while details refresh")
            reply(.playlistInfo, text: id, fresh)
        }
        await model.prepareToQuit()
    }
    func playlistPositionChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item,item],details: .manual); driver = connection; return connection
        })
        let connection = driver!, id = String(repeating: "p",count: 22)
        func reply(_ kind: RemoteCatalogKind, text: String = "", _ collection: RemoteCollection) {
            let query = model.spotifyDetailQueries.values.first { $0.kind == kind && $0.text == text }!
            connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: query.queryId,offset: 0,total: 1,items: [],collections: [collection],issue: nil)))
        }
        model.inspectSpotify(.profile)
        reply(.profile,RemoteCollection(id: "original-listener",title: "Original listener",url: "https://open.spotify.com/",artwork: nil,kind: .profile))
        model.inspectSpotify(.playlistInfo,text: id)
        let playlist = RemoteCollection(id: id,title: "Original versioned playlist",url: "https://open.spotify.com/playlist/" + id,artwork: nil,kind: .playlist,ownerId: "original-listener",collaborative: false)
        reply(.playlistInfo,text: id,playlist)
        model.querySpotify(kind: .playlistItems,text: id,offset: 10)
        try await wait { !model.isLoadingSpotifyPage }
        let query = model.spotifyQuery
        func sparsePage(_ permission: Bool?) -> RemotePage {
            RemotePage(positionEdits: permission,itemPositions: [10,12],provider: "spotify",queryId: query.queryId,offset: 10,total: 13,items: [item,item],collections: [],issue: PlayerIssue(code: "track_unavailable",message: "Original omitted row"))
        }
        connection.receive(.remotePage(sparsePage(true)))
        let before = connection.recorded().count
        model.editSpotify(.init(kind: "playlist_move",id: id,from: 12,to: 10,queryId: query.queryId + 1))
        try check(!model.spotifyEditPending && connection.recorded().count == before, "A context-menu action from a different catalog query is rejected before FFI")
        model.editSpotify(.init(kind: "playlist_move",id: id,from: 11,to: 10,queryId: query.queryId))
        try check(!model.spotifyEditPending && connection.recorded().count == before, "An omitted playlist position cannot become the source of a move")
        connection.receive(.remotePage(sparsePage(nil)))
        model.editSpotify(.init(kind: "playlist_move",id: id,from: 12,to: 10,queryId: query.queryId))
        try check(!model.spotifyEditPending && connection.recorded().count == before, "Legacy or unversioned pages do not admit positional writes")
        connection.receive(.remotePage(sparsePage(true)))
        model.editSpotify(.init(kind: "playlist_move",id: id,from: 12,to: 10,queryId: query.queryId))
        guard case .remote(_, .edit(let move)) = connection.recorded().last else { throw Failure(message: "Missing versioned move") }
        try check(model.spotifyEditPending && move.edit.queryId == query.queryId && move.edit.from == 12, "A duplicate row move carries its page provenance and original sparse position")
        connection.receive(.remoteEdited(RemoteEditResult(operationId: move.operationId,collection: nil,error: PlayerIssue(code: "catalog_changed",message: "This playlist changed. Check its refreshed order before moving again."),saved: [])))
        try check(!model.spotifyEditPending && model.spotifyEditSuccess == nil && model.spotifyEditIssue != nil && model.spotifyQuery.queryId != query.queryId && model.spotifyQuery.offset == 10, "A version conflict refreshes the current page while retaining visible failure feedback")
        try await wait { !model.isLoadingSpotifyPage }
        let editCount = connection.recorded().filter { if case .remote(_, .edit) = $0 { return true }; return false }.count
        try check(editCount == 1, "Conflict recovery sends a fresh read without retrying the playlist mutation")
        try await render(SpotifyEditFeedback(player: model).padding(24).frame(maxWidth: .infinity,maxHeight: .infinity,alignment: .topLeading).background(Color(white: 0.08)),name: "playlist-conflict",size: NSSize(width: 580,height: 150))
        model.querySpotify(kind: .playlistItems,text: id)
        try await wait { !model.isLoadingSpotifyPage }
        model.editSpotify(.init(kind: "playlist_move",id: id,from: 0,to: 2,queryId: model.spotifyQuery.queryId))
        guard case .remote(_, .edit(let second)) = connection.recorded().last else { throw Failure(message: "Missing second move") }
        model.querySpotify(kind: .search,text: "Original")
        let searchQuery = model.spotifyQuery.queryId
        connection.receive(.remoteEdited(RemoteEditResult(operationId: second.operationId,collection: nil,error: PlayerIssue(code: "catalog_changed",message: "Original late conflict"),saved: [])))
        try check(model.spotifyQuery.kind == .search && model.spotifyQuery.queryId == searchQuery, "A late conflict cannot navigate away from the user's newer search")
        await model.prepareToQuit()
    }

    func savedMembershipChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [], mainMode: .manual); driver = connection; return connection
        })
        let connection = driver!
        func deliver(_ query: RemoteQuery, items: [MediaItem] = [], collections: [RemoteCollection] = [], issue: PlayerIssue? = nil) {
            connection.receive(.remotePage(RemotePage(detail: false, provider: "spotify", queryId: query.queryId,
                offset: query.offset, total: UInt64(items.count + collections.count), items: items, collections: collections, issue: issue)))
        }
        func editOperation() throws -> RemoteEditRequest {
            guard case .remote(_, .edit(let request)) = connection.recorded().last else { throw Failure(message: "Missing membership request") }
            return request
        }
        let episode = MediaItem(id: "spotify:episode:" + String(repeating: "e", count: 22),
            title: "Original saved episode", artists: item.artists, album: item.album,
            durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities)
        let collectionKinds: [(RemoteCatalogKind, RemoteEntityKind)] = [
            (.savedAlbums, .album), (.savedShows, .show), (.savedAudiobooks, .audiobook), (.followedArtists, .artist)
        ]
        var cases: [(RemoteCatalogKind, String, [MediaItem], [RemoteCollection])] = [
            (.savedTracks, item.id, [item], []), (.savedEpisodes, episode.id, [episode], [])
        ]
        for (pageKind, entity) in collectionKinds {
            let id = String(repeating: "p", count: 22)
            let collection = RemoteCollection(id: id, title: "Original \(entity.title)", url: "https://open.spotify.com/\(entity.rawValue)/" + id, artwork: nil, kind: entity)
            cases.append((pageKind, collection.uri, [], [collection]))
        }
        for (kind, uri, items, collections) in cases {
            model.querySpotify(kind: kind)
            let earlier = model.spotifyQuery
            model.checkSpotifyURI(uri)
            let checkRequest = try editOperation()
            deliver(earlier, items: items, collections: collections)
            try check(model.spotifySaved[uri] == nil, "A page does not change \(kind.rawValue) membership during its explicit status check")
            connection.receive(.remoteEdited(.init(operationId: checkRequest.operationId, collection: nil, error: nil, saved: [.init(uri: uri, saved: false)])))
            deliver(earlier, items: items, collections: collections)
            try check(model.spotifySaved[uri] == false, "A page requested before a \(kind.rawValue) status check cannot overwrite it")

            model.querySpotify(kind: kind)
            deliver(earlier, items: items, collections: collections)
            deliver(model.spotifyQuery, items: items, collections: collections, issue: PlayerIssue(code: "permission_lost", message: "Original denied page"))
            try check(model.spotifySaved[uri] == false, "Superseded and denied \(kind.rawValue) pages do not change saved status")
            let partial = kind == .savedAlbums ? PlayerIssue(code: "track_unavailable", message: "An unrelated item was omitted") : nil
            deliver(model.spotifyQuery, items: items, collections: collections, issue: partial)
            try check(model.spotifySaved[uri] == true, "A fresh \(kind.rawValue) page corrects an old negative membership check")
            model.querySpotify(kind: kind)
            deliver(model.spotifyQuery)
            try check(model.spotifySaved[uri] == true, "Absence from one \(kind.rawValue) page is not proof of removal")
        }

        model.querySpotify(kind: .savedTracks)
        let beforeRemoval = model.spotifyQuery
        model.saveSpotifyURI(item.id, save: false)
        let removal = try editOperation()
        connection.receive(.remoteEdited(.init(operationId: removal.operationId, collection: nil, error: nil, saved: [])))
        let automaticReload = model.spotifyQuery
        try check(automaticReload.queryId > beforeRemoval.queryId && model.spotifySaved[item.id] == false, "Removal acknowledges the change and requests the existing catalog reload")
        deliver(beforeRemoval, items: [item])
        deliver(automaticReload, items: [item])
        connection.receive(.remoteEdited(.init(operationId: removal.operationId, collection: nil, error: nil, saved: [.init(uri: item.id, saved: true)])))
        try check(model.spotifySaved[item.id] == false, "Neither a lagged automatic reload nor a duplicate edit result restores an acknowledged removal")

        model.querySpotify(kind: .savedTracks)
        model.checkSpotifyURI(item.id)
        let failure = PlayerIssue(code: "rate_limited", message: "Original rejected status check")
        connection.receive(.commandResult(.init(requestId: connection.lastRequestID(), error: failure)))
        deliver(model.spotifyQuery, items: [item])
        try check(model.spotifySaved[item.id] == true, "A failed status check leaves a fresh saved page free to correct membership")
        await model.prepareToQuit()
    }
    func favoriteChecks(_ item: MediaItem) async throws {
        let suite = "app.auvra.favorites-fixture." + UUID().uuidString
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let preferences = SpotifyPreferences(defaults: defaults)
        let a = String(repeating: "a", count: 22), b = String(repeating: "b", count: 22), c = String(repeating: "c", count: 22)
        let accountA = SpotifyPreferences.Account(profileID: "account-a", stableID: "stable-a")
        let legacyA = SpotifyPreferences.Account(profileID: accountA.profileID)
        let legacyB = SpotifyPreferences.Account(profileID: "account-b")
        let stableTextCollision = SpotifyPreferences.Account(profileID: "stable-a")
        func playlist(_ id: String, _ title: String, owner: String? = nil, collaborative: Bool? = nil, public isPublic: Bool? = true) -> RemoteCollection {
            RemoteCollection(id: id, title: title, url: "https://open.spotify.com/playlist/" + id, artwork: "https://i.scdn.co/image/original-temporary-image", isPublic: isPublic, kind: .playlist, ownerId: owner, collaborative: collaborative)
        }
        let oldRecord = try JSONSerialization.data(withJSONObject: ["version": 1, "pins": [
            ["accountID": legacyA.profileID, "playlistID": a, "title": "Любимая музыка A"],
            ["accountID": legacyB.profileID, "playlistID": b, "title": "Favorite B"],
            ["accountID": stableTextCollision.profileID, "playlistID": c, "title": "Same identifier text"]
        ]])
        defaults.set(oldRecord, forKey: SpotifyPreferences.key)
        let reopened = SpotifyPreferences(defaults: defaults)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let decoded = try decoder.decode(RemoteCollection.self,from: Data("{\"id\":\"\(a)\",\"title\":\"Original JSON playlist\",\"url\":\"https://open.spotify.com/\",\"kind\":\"playlist\",\"owner_id\":\"account-a\",\"collaborative\":true}".utf8))
        try check(decoded.ownerId == "account-a" && decoded.collaborative == true, "Playlist ownership fields survive the real JSON key conversion at the host boundary")
        try check(reopened.favorite(for: legacyA)?.title == "Любимая музыка A" && reopened.favorite(for: legacyB)?.playlistID == b,
                  "Version 1 pins restore separately when Spotify supplies only a profile ID")
        let unverifiedAlias = SpotifyPreferences.Account(profileID: "different-profile", stableID: "account-a")
        try check(reopened.favorite(for: unverifiedAlias) == nil && defaults.data(forKey: SpotifyPreferences.key) == oldRecord,
                  "A stable ID that happens to equal another profile ID cannot promote its old pin")
        defaults.set(c,forKey: "AuvraFavoritePlaylistID")
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item],details: .manual); driver = connection; return connection
        },spotifyPreferences: reopened)
        let connection = driver!
        try check(model.favoritePlaylist == nil && model.spotifyProfileID == nil, "A cold start cannot display an unverified or legacy account favorite")
        func reply(_ kind: RemoteCatalogKind, text: String = "", collections: [RemoteCollection]) {
            let query = model.spotifyDetailQueries.values.first { $0.kind == kind && $0.text == text }!
            connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: query.queryId,offset: 0,total: 1,items: [],collections: collections,issue: nil)))
        }
        model.inspectSpotify(.profile)
        let oldProfile = model.spotifyDetailQueries.values.first!.queryId
        let profile = try decoder.decode(RemoteCollection.self, from: Data("{\"id\":\"account-a\",\"account_id\":\"stable-a\",\"title\":\"Original account A\",\"url\":\"https://open.spotify.com/\",\"kind\":\"profile\"}".utf8))
        reply(.profile, collections: [profile])
        try check(profile.accountId == accountA.stableID && model.spotifyProfileID == accountA.profileID
                  && model.favoritePlaylistID == a && model.favoritePlaylistTitle == "Любимая музыка A"
                  && model.isLoadingSpotifyDetail(.playlistInfo,text: a),
                  "Verified profile promotes its own favorite to the immutable account ID and refreshes its metadata")
        let renamedA = SpotifyPreferences.Account(profileID: "renamed-account-a", stableID: accountA.stableID)
        let differentAccount = SpotifyPreferences.Account(profileID: accountA.profileID, stableID: "stable-other")
        try check(renamedA.key == accountA.key && stableTextCollision.key != accountA.key
                  && reopened.favorite(for: renamedA)?.playlistID == a && reopened.favorite(for: stableTextCollision)?.playlistID == c
                  && reopened.favorite(for: legacyA) == nil && reopened.favorite(for: differentAccount) == nil,
                  "Stable pins survive a profile rename and stay separate from legacy and other stable accounts")
        try check(reopened.setFavorite(nil, for: differentAccount) && reopened.favorite(for: accountA)?.playlistID == a,
                  "A different stable account cannot unpin the favorite even when the profile ID is the same")
        let own = playlist(a,"Updated original favorite",owner: "account-a",collaborative: false)
        let navigation = AppNavigation(); navigation.openCollection(own)
        navigation.refreshCollection(RemoteCollection(id: a,title: "Updated original title",url: own.url,artwork: nil))
        try check(navigation.selectedCollection?.ownerId == own.ownerId && navigation.selectedCollection?.collaborative == false, "Partial collection UI updates preserve ownership metadata for the selected playlist")
        reply(.playlistInfo,text: a,collections: [own])
        try check(model.favoritePlaylist?.artwork == own.artwork && reopened.favorite(for: accountA)?.title == own.title, "Fresh playlist metadata updates favorite cover in memory and persists its new title")
        let saved = String(data: defaults.data(forKey: SpotifyPreferences.key)!, encoding: .utf8)!
        try check(!saved.contains("https:") && !saved.contains("artwork"), "Favorite preferences omit expiring cover URLs and remote page metadata")
        try check(model.playlistEditAccess(own) == .owner, "Playlist owner can edit its items and details regardless of public visibility")
        model.inspectSpotify(.playlistInfo,text: b)
        let foreign = playlist(b,"Original foreign playlist",owner: "account-b",collaborative: false)
        reply(.playlistInfo,text: b,collections: [foreign])
        try check(model.playlistEditAccess(foreign) == .readOnly, "A public playlist from another owner stays read only")
        let commandsBefore = connection.recorded().count
        model.editSpotify(.init(kind: "playlist_replace",uris: [item.id],id: b))
        try check(!model.spotifyEditPending && model.spotifyEditIssue != nil && connection.recorded().count == commandsBefore, "Store rejects a forbidden playlist write before submitting any provider command")
        model.inspectSpotify(.playlistInfo,text: c)
        let collaboration = playlist(c,"Original collaboration",owner: "another-owner",collaborative: true,public: false)
        reply(.playlistInfo,text: c,collections: [collaboration])
        try check(model.playlistEditAccess(collaboration).canEditItems && !model.playlistEditAccess(collaboration).canEditDetails, "Collaboration permits item changes without granting owner-only name or cover actions")
        model.editSpotify(.init(kind: "playlist_details",id: c,name: "Forbidden rename"))
        try check(!model.spotifyEditPending && model.spotifyEditIssue != nil, "Collaborative access cannot rename a playlist through the shared Store")
        let beforeAccessChange = connection.recorded().count
        model.editSpotify(.init(kind: "playlist_details", id: c, public: false, collaborative: true))
        try check(!model.spotifyEditPending && model.spotifyEditIssue != nil && connection.recorded().count == beforeAccessChange,
                  "A collaborator cannot change owner-only collaboration settings")
        model.inspectSpotify(.playlistInfo,text: c,refresh: true)
        let failedRefresh = model.spotifyDetailQueries.values.first { $0.kind == .playlistInfo }!
        connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: failedRefresh.queryId,offset: 0,total: 0,items: [],collections: [],issue: PlayerIssue(code: "permissions",message: "Original permissions refresh failure"))))
        try check(model.playlistEditAccess(collaboration) == .unknown && model.playlistEditAccess(id: c) == .unknown, "A failed permissions refresh cannot reuse old editing rights from the selected collection")
        for number in 0..<34 {
            let id = String(format: "%022d",number)
            model.inspectSpotify(.artist,text: id)
            reply(.artist,text: id,collections: [RemoteCollection(id: id,title: "Original cache fixture",url: "https://open.spotify.com/artist/" + id,artwork: nil,kind: .artist)])
        }
        try check(model.spotifyDetails.count == 32 && model.spotifyProfileID == "account-a", "The bounded metadata cache retains verified identity while evicting other details")
        model.editSpotify(.init(kind: "playlist_add",uris: [item.id],id: a))
        let admitted = connection.recorded().compactMap { command -> RemoteEditRequest? in if case .remote(_, .edit(let request)) = command { return request }; return nil }.last!
        try check(model.spotifyEditPending, "Verified owner access admits an explicit playlist item operation")
        connection.receive(.remoteState(RemoteState(provider: "spotify",connection: .disconnected,authenticationRequired: true,issue: nil,retryAfterSeconds: nil,devices: [],playback: nil)))
        connection.receive(.remoteEdited(RemoteEditResult(operationId: admitted.operationId,collection: own,error: nil,saved: [])))
        try check(!model.spotifyEditPending && model.spotifyCreatedPlaylist == nil && model.spotifyEditSuccess == nil && model.spotifyEditIssue?.contains("before Spotify confirmed") == true, "A late write result after authorization revocation cannot restore account data or claim success")
        model.acceptSpotifyAccess("original-test-credential")
        try check(model.favoritePlaylist == nil && model.playlistEditAccess(own) == .unknown, "Credential generation change immediately hides the previous favorite and edit access")
        connection.receive(.remotePage(RemotePage(detail: true,provider: "spotify",queryId: oldProfile,offset: 0,total: 1,items: [],collections: [RemoteCollection(id: "account-a",title: "Stale account A",url: "https://open.spotify.com/",artwork: nil,kind: .profile)],issue: nil)))
        try check(model.favoritePlaylist == nil, "A late profile cannot restore the previous account favorite")
        reply(.profile,collections: [RemoteCollection(id: "account-b",title: "Original account B",url: "https://open.spotify.com/",artwork: nil,kind: .profile)])
        try check(model.favoritePlaylistID == b && model.favoritePlaylistTitle == "Favorite B", "Switching verified accounts restores the new account's own favorite")
        model.pinPlaylist(nil)
        try check(reopened.favorite(for: legacyB) == nil && reopened.favorite(for: accountA)?.playlistID == a, "Unpin removes only the current account preference")
        try check(preferences.setFavorite(playlist(b, "Legacy fallback alias"), for: legacyA)
                  && reopened.favorite(for: accountA)?.playlistID == a && reopened.favorite(for: legacyA) == nil,
                  "An existing stable pin wins over a legacy alias and removes the alias on read")
        try check(preferences.setFavorite(playlist(b, "Second fallback alias"), for: legacyA)
                  && reopened.setFavorite(nil, for: accountA)
                  && preferences.favorite(for: legacyA) == nil && preferences.favorite(for: accountA) == nil,
                  "Unpin removes the legacy alias with the stable pin so it cannot return")
        let invalid = SpotifyPreferences.Account(profileID: accountA.profileID, stableID: "invalid stable identifier")
        try check(!preferences.setFavorite(playlist(a, "Invalid identity"), for: invalid)
                  && !preferences.setFavorite(decoded, for: SpotifyPreferences.Account(profileID: "")),
                  "Invalid account identities cannot write preferences")
        for index in 0..<12 {
            preferences.setFavorite(playlist(a,"Original bounded fixture"), for: SpotifyPreferences.Account(profileID: "bounded-\(index)"))
        }
        let boundedFirst = SpotifyPreferences.Account(profileID: "bounded-0"), boundedLast = SpotifyPreferences.Account(profileID: "bounded-11")
        try check(preferences.favorite(for: boundedFirst) == nil && preferences.favorite(for: boundedLast) != nil, "Favorite storage retains at most eight account records")
        defaults.set(Data(repeating: 0,count: 16 * 1024 + 1),forKey: SpotifyPreferences.key)
        try check(preferences.favorite(for: boundedLast) == nil, "Malformed or oversized preferences do not restore unchecked favorite data")
        await model.prepareToQuit()
    }
    func restoredAccountDiscoveryCancellationChecks(_ item: MediaItem) async throws {
        let process = FixtureProcess()
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var fixture: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item], details: .manual, acceptsRestoreCancellation: true)
            fixture = connection; return connection
        }, playbackRuntime: runtime, lyricsProvider: OriginalLyricsFixture())
        let driver = fixture!
        let context = "spotify:album:" + String(repeating: "c", count: 22)
        project(driver, frame(item, playing: false, context: context, position: 42_000), acknowledge: false,
                restored: RestoredRemote(contextUri: context))
        try check(model.spotifyProfileID == nil, "The cancellation regression begins before the first profile result")
        model.togglePlayback(); model.togglePlayback(); model.togglePlayback()
        guard let query = driver.recorded().compactMap({ command -> RemoteQuery? in
            if case .remote(_, .query(let query)) = command, query.kind == .profile { return query }; return nil
        }).last else { throw Failure(message: "Missing profile discovery query") }
        let profile = RemoteCollection(id: "original-listener", title: "Original fixture listener", url: "https://open.spotify.com/user/original-listener", artwork: nil, kind: .profile)
        driver.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: query.queryId,
            offset: 0, total: 1, items: [], collections: [profile], issue: nil)))
        try check(model.spotifyProfileID == "original-listener", "Initial profile discovery resolves during Resume cancellation")
        try await Task.sleep(for: .milliseconds(100))
        let resumes = driver.recorded().compactMap { command -> UInt64? in
            if case .remote(_, .control(.resumeOnDevice(_, _, _, _, let operation))) = command { return operation }; return nil
        }
        try check(resumes.count == 2 && resumes[0] != resumes[1] && model.spotifyStartingItemID == item.id,
                  "Resolving the first account identity does not cancel the new Resume after a third toggle")
        await model.prepareToQuit()
    }
    func restoredRemoteChecks(_ item: MediaItem) async throws {
        let context = "spotify:playlist:" + String(repeating: "p",count: 22)
        func restorePauses(_ connection: FixtureConnection) -> [(operation: UInt64, device: String)] {
            connection.recorded().compactMap { command in
                if case .pauseConfirmedRestoredResume(let operation, let device) = command { return (operation, device) }
                return nil
            }
        }
        func hasUnscopedPause(_ connection: FixtureConnection) -> Bool {
            connection.recorded().contains {
                switch $0 { case .remote(_, .control(.pauseOnDevice)), .pause: true; default: false }
            }
        }
        func snapshot(_ revision: UInt64) -> PlaybackSnapshot {
            PlaybackSnapshot(restoredRemote: RestoredRemote(contextUri: context),revision: revision,state: .paused,positionMs: 42_000,volume: 0.52,activeProvider: "spotify",providerConnection: .connected,
                capabilities: .init(play: true,pause: false,seek: false,volume: false,pcmAnalysis: false,systemMediaSession: false),
                queue: QueueState(revision: revision,context: .userQueue,current: item,upcoming: [],history: [],repeatMode: .off,shuffle: false,playNextCount: 0,forwardCount: 0),
                transport: .init(stop: false,next: false,previous: false,shuffle: false,repeat: false,queueEdit: false),playbackSessionActive: false,
                session: SessionState(enabled: true,ready: true,restored: true,saving: false,issue: nil),queueIssues: [])
        }
        let process = FixtureProcess(); let runtime = SoloistRuntime(credentials: FixtureCredentials(),owner: process,deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item]); driver = connection; return connection
        },playbackRuntime: runtime)
        let connection = driver!; let media = MediaPlayerSystemSession(player: model); model.attachSystemMedia(media)
        connection.receive(.remoteState(frame(item,device: "external",playing: true)))
        connection.receive(.snapshot(snapshot(50_000)))
        try check(model.canTogglePlayback && !model.isPlaying && !model.canSeek && !model.canEditQueue && !media.isPublishing, "Saved remote selection stays paused without claiming output or restoring a local queue")
        try check(model.displayedVolume == nil && abs(model.volume - 0.52) < 0.0001,
                  "A restored Spotify selection does not present the local session's volume as its output level")
        let still = StillStore(); let navigation = AppNavigation(); let windows = MacWindowService()
        try await render(RootView(player: model,navigation: navigation,still: still,windows: windows),name: "spotify-restored",size: NSSize(width: 960,height: 650))
        try await render(MiniPlayerView(player: model,showMain: {}),name: "spotify-restored-mini",size: NSSize(width: 360,height: 264))
        try check(model.position() == 42 && !media.isPublishing, "Saved Spotify position does not advance while the app waits for explicit Play")
        model.togglePlayback()
        try await Task.sleep(for: .milliseconds(150))
        try check(model.spotifyStartingItemID == item.id && !connection.recorded().contains { switch $0 { case .remote(_, .activate), .remote(_, .adoptOutput), .remote(_, .control(.resumeOnDevice)): true; default: false } }, "Resume waits for discovery without dropping the restored selection or sending Play to a stale device")
        try check(model.canTogglePlayback && model.playbackToggleShowsPause, "A restored Play waiting for its device remains cancellable")
        model.togglePlayback()
        connection.receive(.remoteState(frame(item,playing: false,position: 0)))
        try await Task.sleep(for: .milliseconds(150))
        try check(model.spotifyStartingItemID == nil && !model.playbackToggleShowsPause && model.position() == 42
                  && !connection.recorded().contains { switch $0 { case .pause, .remote(_, .control(.resumeOnDevice)): true; default: false } },
                  "Cancelling before device discovery keeps the saved pause and sends no playback command")
        model.togglePlayback()
        try check(model.canTogglePlayback && model.playbackToggleShowsPause, "An already submitted Resume accepts a deferred Pause while playback is pending")
        try await wait { connection.recorded().contains { if case .remote(_, .control(.resumeOnDevice(_,_,_,_,_))) = $0 { return true }; return false } }
        let resumes = connection.recorded().compactMap { command -> (String,UInt64,String?,UInt64)? in
            if case .remote(_, .control(.resumeOnDevice(let selected,let device,let position,let context,let operation))) = command,
               selected.id == item.id, let operation { return (device,position,context,operation) }; return nil
        }
        try check(resumes.count == 1 && resumes[0].0 == "owned-output" && resumes[0].1 == 42_000 && resumes[0].2 == context, "Explicit Resume sends the saved position and context once to the discovered owned output")
        model.togglePlayback()
        try check(model.canTogglePlayback && !model.playbackToggleShowsPause, "A second toggle defers Pause")
        model.togglePlayback()
        try check(model.canTogglePlayback && model.playbackToggleShowsPause, "Another toggle cancels the deferred Pause")
        model.togglePlayback()
        connection.receive(.remoteState(frame(item,device: "external",playing: true,position: 42_000)))
        try check(model.spotifyStartingItemID == item.id && !media.isPublishing, "The same track on a phone cannot confirm an owned-output resume")
        connection.receive(.remoteState(frame(item,playing: true,position: 1000)))
        try check(model.spotifyStartingItemID == item.id && !media.isPublishing && restorePauses(connection).isEmpty && !hasUnscopedPause(connection),
                  "An external or wrong-position observation cannot send the deferred Pause")
        try check(!connection.recorded().contains { if case .remote(_, .adoptOutput) = $0 { return true }; return false }, "Automatic owned-output projection cannot cancel a pending restored-position confirmation")
        connection.receive(.remoteState(frame(item,playing: true,position: 42_500)))
        revision = 50_001; project(connection,frame(item,playing: true,position: 42_500,volume: 0.73))
        try check(model.displayedVolume.map { abs($0 - 0.73) < 0.0001 } == true
                  && !connection.recorded().contains { if case .setVolume = $0 { return true }; return false },
                  "Confirmed remote volume replaces the restored placeholder without sending a volume command")
        let pauses = restorePauses(connection)
        try check(model.spotifyStartingItemID == nil && pauses.count == 1 && pauses.first?.operation == resumes.first?.3
                  && pauses.first?.device == "owned-output" && !hasUnscopedPause(connection) && !media.isPublishing,
                  "Confirmed Resume dispatches one scoped Pause for the same operation and device without publishing a transient session")
        project(connection,frame(item,playing: true,position: 42_700))
        try check(restorePauses(connection).count == 1 && !hasUnscopedPause(connection) && !media.isPublishing,
                  "A Playing heartbeat while the targeted Pause is in flight cannot claim the output")
        project(connection,frame(item,playing: false,position: 42_700))
        try check(!model.isPlaying && !media.isPublishing, "The targeted Pause completes the restored transition")
        await model.prepareToQuit(); await still.shutdown()
        let rejectedProcess = FixtureProcess(); let rejectedRuntime = SoloistRuntime(credentials: FixtureCredentials(),owner: rejectedProcess,deviceName: "Auvra fixture")
        rejectedRuntime.restore(); try await wait { rejectedRuntime.status.phase == .ready }
        var rejectedDriver: FixtureConnection?
        let rejected = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item],rejectResume: true); rejectedDriver = connection; return connection
        },playbackRuntime: rejectedRuntime)
        rejectedDriver!.receive(.remoteState(frame(item,playing: false))); rejectedDriver!.receive(.snapshot(snapshot(60_000)))
        rejected.togglePlayback(); try await wait { rejected.errorMessage == "Original saved selection rejection" }
        try check(!rejected.isLoading && rejected.current?.id == item.id, "A rejected Resume ends pending feedback immediately and keeps the saved selection")
        await rejected.prepareToQuit()
        for resumeAgain in [false, true] {
            let process = FixtureProcess(); let runtime = SoloistRuntime(credentials: FixtureCredentials(),owner: process,deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let player = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive,items: [item],acceptsRestoreCancellation: true)
                fixture = connection
                return connection
            },playbackRuntime: runtime)
            let connection = fixture!
            connection.receive(.remoteState(frame(item,playing: false))); connection.receive(.snapshot(snapshot(70_000)))
            func resumeOperations() -> [UInt64] {
                connection.recorded().compactMap { command in
                    if case .remote(_, .control(.resumeOnDevice(_,_,_,_,let operation))) = command { return operation }
                    return nil
                }
            }
            player.togglePlayback(); player.togglePlayback()
            if resumeAgain { player.togglePlayback() }
            try await wait { resumeAgain ? resumeOperations().count == 2 : player.spotifyStartingItemID == nil }
            let operations = resumeOperations()
            let canceled = connection.recorded().compactMap { command -> UInt64? in
                if case .cancelRestoredResume(let operation, let device) = command, device == "owned-output" { return operation }
                return nil
            }
            try check(operations.count == (resumeAgain ? 2 : 1) && canceled == [operations[0]]
                      && (resumeAgain ? operations[0] != operations[1] : player.position() == 42 && !player.playbackToggleShowsPause)
                      && restorePauses(connection).isEmpty && !hasUnscopedPause(connection),
                      resumeAgain ? "A third toggle resumes once with a new operation after successful cancellation"
                                     : "Successful early cancellation leaves the saved selection paused without sending Pause")
            await player.prepareToQuit()
        }
        for queuedPause in [false, true] {
            let lateProcess = FixtureProcess(); let lateRuntime = SoloistRuntime(credentials: FixtureCredentials(),owner: lateProcess,deviceName: "Auvra fixture")
            lateRuntime.restore(); try await wait { lateRuntime.status.phase == .ready }
            var lateDriver: FixtureConnection?
            let late = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive,items: [item]); lateDriver = connection; return connection
            },playbackRuntime: lateRuntime,remoteConfirmationTimeout: .milliseconds(30))
            let lateMedia = MediaPlayerSystemSession(player: late); late.attachSystemMedia(lateMedia)
            lateDriver!.receive(.remoteState(frame(item,playing: false))); lateDriver!.receive(.snapshot(snapshot(80_000)))
            try await wait { late.spotifyProfileID != nil }
            late.togglePlayback()
            if queuedPause { late.togglePlayback() }
            try await wait { !late.isLoading && late.errorMessage?.contains("not been confirmed") == true }
            lateDriver!.receive(.remoteState(frame(item,device: "external",playing: true,position: 42_000)))
            lateDriver!.receive(.remoteState(frame(item,playing: true,position: 1000)))
            try check(!late.isPlaying && !lateMedia.isPublishing && late.snapshot?.restoredRemote != nil
                      && restorePauses(lateDriver!).isEmpty && !hasUnscopedPause(lateDriver!)
                      && !lateDriver!.recorded().contains { if case .remote(_, .adoptOutput) = $0 { return true }; return false },
                      "A late wrong device or position cannot publish output or send deferred Pause")
            revision = 80_001; project(lateDriver!,frame(item,playing: true,position: 42_500))
            if queuedPause {
                let operation = lateDriver!.recorded().compactMap { command -> UInt64? in
                    if case .remote(_, .control(.resumeOnDevice(_,_,_,_,let operation))) = command { return operation }
                    return nil
                }.first
                let pauses = restorePauses(lateDriver!)
                try check(operation != nil && pauses.count == 1 && pauses.first?.operation == operation && pauses.first?.device == "owned-output"
                          && !hasUnscopedPause(lateDriver!) && !lateMedia.isPublishing && late.errorMessage == nil,
                          "An exact late Resume applies its original scoped Pause once after the visible timeout")
            } else {
                try check(late.isPlaying && lateMedia.isPublishing && late.errorMessage == nil, "A late confirmed Resume clears its timeout and acquires the system session")
            }
            await late.prepareToQuit()
        }

        for rejectInHelper in [false, true] {
            let staleProcess = FixtureProcess(clockSupported: rejectInHelper,controlSupported: rejectInHelper)
            let staleRuntime = SoloistRuntime(credentials: FixtureCredentials(),owner: staleProcess,deviceName: "Auvra fixture")
            staleRuntime.restore(); try await wait { staleRuntime.status.phase == .ready }
            var staleDriver: FixtureConnection?
            let stale = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive,items: [item],rejectConfirmedRestoredPause: !rejectInHelper)
                staleDriver = connection
                return connection
            },playbackRuntime: staleRuntime)
            let staleConnection = staleDriver!, staleMedia = MediaPlayerSystemSession(player: stale)
            stale.attachSystemMedia(staleMedia)
            staleConnection.receive(.remoteState(frame(item,playing: false))); staleConnection.receive(.snapshot(snapshot(90_000)))
            if rejectInHelper {
                // This checks rejection after Resume submission. Start from an
                // already active paused endpoint; inactive bootstrap has its
                // separate cancellation-before-submission regression matrix.
                await staleProcess.clock(item.id, sequence: 1, state: "paused", rate: 0, position: 42_000, queueEpoch: 4)
                try await wait { staleRuntime.clock?.sequence == 1 }
            }
            stale.togglePlayback(); stale.togglePlayback()
            if rejectInHelper {
                await staleProcess.clock(item.id, sequence: 2, rate: 1, position: 42_500, queueEpoch: 4)
                try await wait { staleRuntime.clock?.sequence == 2 }
            }
            var playing = frame(item,playing: true,position: 42_500,ownedOutputObserved: rejectInHelper ? true : nil)
            if rejectInHelper { playing.ownedOutputControl = true }
            revision = 90_001; project(staleConnection,playing)
            try check(restorePauses(staleConnection).count == 1 && !staleMedia.isPublishing,
                      "The scoped Pause suppresses a transient native session while its result is pending")
            let rejectionDeadline = ContinuousClock.now.advanced(by: .seconds(1))
            if rejectInHelper {
                let request = staleConnection.lastRequestID(matching: { if case .pauseConfirmedRestoredResume = $0 { return true }; return false })
                guard let request else { throw Failure(message: "Missing scoped Pause request for helper fixture") }
                await staleProcess.rejectNextControl()
                let output = RemoteOutputCommand(id: 91, generation: staleRuntime.clockGeneration, sequence: 2,
                    expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900, requestId: request,
                    requiredQueueEpoch: 4, deviceId: "owned-output", mediaId: item.id,
                    control: .init(kind: .pause, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
                staleConnection.receive(.remoteOutputCommand("spotify",output))
                try await wait {
                    staleConnection.recorded().contains { command in
                        if case .remote(_, .outputCommandResult(let result)) = command { return result.id == 91 && !result.accepted }
                        return false
                    }
                }
                let sent = await staleProcess.sentControlCount
                try check(sent == 1, "The verified scoped command reaches the mock helper and reports its rejection")
            }
            try await wait { staleMedia.isPublishing }
            try check(ContinuousClock.now < rejectionDeadline && restorePauses(staleConnection).count == 1 && !hasUnscopedPause(staleConnection),
                      rejectInHelper ? "A rejected helper Pause restores ownership without a second send"
                                     : "A stale scoped Pause releases ownership suppression without replaying Pause")
            await stale.prepareToQuit()
        }
    }
    func controlResultChecks(_ item: MediaItem) async throws {
        let process = FixtureProcess(); let runtime = SoloistRuntime(credentials: FixtureCredentials(),owner: process,deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        let next = track("b",title: "Another original selection")
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item,next]); driver = connection; return connection
        },playbackRuntime: runtime)
        let connection = driver!
        func operation() -> UInt64 { connection.recorded().reversed().compactMap { command -> UInt64? in if case .remote(_, .control(let control)) = command { return control.operationId }; return nil }.first! }
        project(connection,frame(item,position: 65_000))
        model.startSpotifyTrack(item)
        let first = operation()
        project(connection,frame(item,position: 65_000),acknowledge: false)
        try check(model.isLoading, "A cached Playing frame cannot confirm restarting the same track before HTTP dispatch")
        model.startSpotifyTrack(next)
        let second = operation()
        var old = frame(item,position: 65_000); old.controlResult = RemoteControlResult(operationId: first,error: PlayerIssue(code: "permission_lost",message: "Original previous request failure"))
        connection.receive(.remoteState(old))
        try check(first != second && model.isLoading && model.errorMessage == nil && model.spotifyStartingItemID == next.id, "An old HTTP rejection cannot cancel or label a newer track request")
        var rejected = frame(item,position: 65_000); rejected.controlResult = RemoteControlResult(operationId: second,error: PlayerIssue(code: "permission_lost",message: "Original current request rejection"))
        connection.receive(.remoteState(rejected))
        try check(!model.isLoading && model.errorMessage == "Original current request rejection" && model.current?.id == item.id, "A matching server rejection ends pending feedback immediately without replacing the actual track")
        model.startSpotifyTrack(next)
        let third = operation()
        var accepted = frame(next,position: 65_000); accepted.controlResult = RemoteControlResult(operationId: third,error: nil)
        project(connection,accepted,acknowledge: false)
        try check(model.isLoading && model.spotifyStartingItemID == next.id, "HTTP acceptance and stale same-track position do not confirm a new start")
        project(connection,frame(next,position: 1250))
        try check(!model.isLoading && model.errorMessage == nil && model.current?.id == next.id, "A dispatched request finishes only when its device, item and start position converge")
        connection.receive(.remoteState(rejected))
        try check(model.errorMessage == nil && !model.isLoading, "Repeated old rejection snapshots cannot resurrect a completed error")
        model.startSpotifyTrack(item)
        model.togglePlayback()
        project(connection,frame(item,position: 1250))
        try check(connection.recorded().contains { if case .remote(_, .control(.pauseOnDevice("owned-output"))) = $0 { return true }; return false }
                  && !connection.recorded().contains { if case .pauseConfirmedRestoredResume = $0 { return true }; return false },
                  "A queued Pause after a catalog start still uses the ordinary device control")
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let decoded = try decoder.decode(RemoteState.self,from: Data(#"{"provider":"spotify","connection":"connected","authentication_required":false,"issue":null,"retry_after_seconds":null,"devices":[],"playback":null,"control_result":{"operation_id":41,"error":{"code":"permission_lost","message":"Original HTTP rejection"}}}"#.utf8))
        try check(decoded.controlResult?.operationId == 41 && decoded.controlResult?.error?.message == "Original HTTP rejection", "The additive correlated result decodes through the real snake-case wire contract")
        await model.prepareToQuit()
    }
    func transferResultChecks(_ item: MediaItem) async throws {
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: FixtureProcess(), deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let value = FixtureConnection(receive, items: [item]); driver = value; return value
        }, playbackRuntime: runtime, remoteConfirmationTimeout: .milliseconds(120))
        let connection = driver!, session = MediaPlayerSystemSession(player: model)
        model.attachSystemMedia(session)
        func observe(_ device: String, _ observation: UInt64, playing: Bool = false, active: Bool = true,
                     receipt: RemoteControlResult? = nil) {
            var state = frame(item, device: device, playing: playing, active: active)
            state.observationId = observation; state.controlResult = receipt
            project(connection, state, acknowledge: false)
        }
        func latestTransfer(_ target: String) throws -> UInt64 {
            guard let operation = connection.recorded().reversed().compactMap({ command -> UInt64? in
                if case .remote(_, .control(.transfer(let device, let operation))) = command, device == target { return operation }
                return nil
            }).first else { throw Failure(message: "Missing tracked transfer to \(target)") }
            return operation
        }
        observe("owned-output", 1, playing: true)
        observe("owned-output", 2)
        model.transferSpotify("external-first")
        let first = try latestTransfer("external-first")
        let commandCount = connection.recorded().count
        model.startSpotifyTrack(item)
        try check(model.selectedSpotifyDeviceID == nil && !session.isPublishing && connection.recorded().count == commandCount,
                  "A pending transfer holds playback selection and temporarily releases the old system lease")
        let denied = PlayerIssue(code: "permission_lost", message: "Original rejected device transfer")
        observe("owned-output", 3, receipt: .init(operationId: first, error: denied))
        try check(model.selectedSpotifyDeviceID == nil && session.isPublishing && !model.isPlaying && model.errorMessage == denied.message,
                  "A matching rejected transfer restores the unchanged owned paused session and reports the failure")
        model.startSpotifyTrack(item)
        try check(connection.recorded().contains { if case .remote(_, .control(.startOnDevice(_, "owned-output", _))) = $0 { return true }; return false },
                  "Play after a rejected device transfer still targets the previously confirmed route")
        model.send(.pause)

        model.transferSpotify("external-second")
        let second = try latestTransfer("external-second")
        observe("external-second", 3, receipt: .init(operationId: second, error: nil))
        observe("external-second", 4, active: false)
        try check(model.selectedSpotifyDeviceID == nil, "A transfer receipt cannot confirm stale or inactive target frames")
        observe("external-second", 5)
        try check(model.selectedSpotifyDeviceID == "external-second" && !session.isPublishing && !model.isPlaying,
                  "Only a newer active observation commits the acknowledged device while preserving Pause")

        model.transferSpotify("external-third")
        let third = try latestTransfer("external-third")
        model.transferSpotify("external-fourth")
        let fourth = try latestTransfer("external-fourth"), fourthRequest = connection.lastRequestID()
        observe("external-third", 6, receipt: .init(operationId: third, error: nil))
        try check(third != fourth && model.selectedSpotifyDeviceID == "external-second", "A superseded transfer receipt cannot commit a device")
        connection.receive(.commandResult(.init(requestId: fourthRequest, error: denied)))
        try check(model.selectedSpotifyDeviceID == "external-second", "A matching admission failure preserves the previous device preference")

        model.transferSpotify("external-timeout")
        let timeout = try latestTransfer("external-timeout")
        observe("external-second", 7, receipt: .init(operationId: timeout, error: nil))
        try await wait { model.errorMessage == "Spotify has not confirmed the selected playback device." }
        try check(model.selectedSpotifyDeviceID == "external-second", "An unconfirmed transfer times out without changing the confirmed route")

        observe("owned-output", 8)
        revision += 1
        connection.receive(.snapshot(PlaybackSnapshot(revision: revision, state: .paused, positionMs: 0, volume: 1,
            activeProvider: "local", providerConnection: .connected, capabilities: capabilities,
            queue: QueueState(revision: revision, context: .userQueue, current: nil, upcoming: [], history: [],
                              repeatMode: .off, shuffle: false, playNextCount: 0, forwardCount: 0), transport: .local,
            playbackSessionActive: true, session: SessionState(enabled: false, ready: true, restored: false, saving: false, issue: nil), queueIssues: [])))
        let beforeSourceSwitch = connection.recorded().count
        model.useAuvraPlayback()
        let sourceSwitch = connection.recorded().dropFirst(beforeSourceSwitch)
        try check(model.selectedSpotifyDeviceID == nil
                  && sourceSwitch.contains { if case .remote(_, .activate) = $0 { return true }; return false }
                  && !sourceSwitch.contains { if case .remote(_, .control(.transfer)) = $0 { return true }; return false },
                  "Selecting the already active Spotify output from local playback activates its source without another transfer")
        await model.prepareToQuit()
    }
    func contextRowStartChecks(_ item: MediaItem) async throws {
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: FixtureProcess(), deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        let other = track("x", title: "Original next row")
        let unplayable = track("u", playable: false)
        let playlist = RemoteCollection(id: String(repeating: "p", count: 22), title: "Original playlist", url: "", artwork: nil, kind: .playlist)
        let album = RemoteCollection(id: String(repeating: "a", count: 22), title: "Original album", url: "", artwork: nil, kind: .album)
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item, other, unplayable], mainMode: .manual)
            driver = connection; return connection
        }, playbackRuntime: runtime, remoteConfirmationTimeout: .milliseconds(250))
        let connection = driver!, media = MediaPlayerSystemSession(player: model)
        model.attachSystemMedia(media)
        func showPage(_ collection: RemoteCollection, items: [MediaItem], positions: [UInt32], offset: UInt32 = 0) -> UInt64 {
            model.querySpotify(kind: collection.childrenKind, text: collection.id, offset: offset)
            let queryID = model.spotifyQuery.queryId
            connection.receive(.remotePage(RemotePage(itemPositions: positions, provider: "spotify", queryId: queryID, offset: offset,
                total: UInt64(positions.last ?? 0) + 1, items: items, collections: [], issue: nil)))
            return queryID
        }
        func controls() -> [RemoteControl] {
            connection.recorded().compactMap { if case .remote(_, .control(let control)) = $0 { return control }; return nil }
        }
        func observe(_ serial: UInt64, _ current: MediaItem, _ context: String?, device: String = "owned-output", active: Bool = true,
                     ownedControl: Bool = false, pauseCapability: Bool = true, restored: RestoredRemote? = nil) {
            var state = frame(current, device: device, context: context, active: active, pauseCapability: pauseCapability)
            state.observationId = serial
            state.ownedOutputControl = ownedControl
            project(connection, state, restored: restored)
        }
        func pauses() -> Int {
            connection.recorded().filter { if case .remote("spotify", .control(.pauseOnDevice("owned-output"))) = $0 { return true }; return false }.count
        }
        observe(10, item, playlist.uri)
        let playlistQuery = showPage(playlist, items: [other, item, unplayable, item], positions: [10, 11, 12, 14], offset: 10)
        let initialCount = controls().count
        model.playSpotifyCatalogItem(item, in: playlist, queryId: playlistQuery, index: 0)
        model.playSpotifyCatalogItem(item, in: album, queryId: playlistQuery, index: 1)
        model.playSpotifyCatalogItem(unplayable, in: playlist, queryId: playlistQuery, index: 2)
        try check(controls().count == initialCount, "Only a playable, matching row from the current collection page may start contextual playback")

        model.playSpotifyCatalogItem(item, in: playlist, queryId: playlistQuery, index: 3)
        guard let control = controls().last,
              case .startContextItem(let selected, let context, let device, let operation, let row) = control else {
            throw Failure(message: "The current playlist row did not send a contextual item start")
        }
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let wire = try JSONSerialization.jsonObject(with: encoder.encode(control)) as! [String: Any]
        try check(selected.id == item.id && context == playlist.uri && device == "owned-output" && operation > 0
                  && wire["kind"] as? String == "start_context" && wire["uri"] as? String == playlist.uri
                  && wire["device_id"] as? String == device && wire["operation_id"] as? UInt64 == operation
                  && row.position == 14 && row.queryId == playlistQuery
                  && wire["position"] as? UInt32 == row.position && wire["query_id"] as? UInt64 == row.queryId
                  && (wire["item"] as? [String: Any])?["id"] as? String == item.id,
                  "The duplicate on the second playlist page sends its actual row and query identity")
        observe(10, item, playlist.uri)
        try check(model.isLoading && !media.isPublishing, "A matching cached observation plus HTTP acceptance cannot confirm a context row start")
        observe(11, item, album.uri)
        try check(model.isLoading && !media.isPublishing, "The right song in the wrong collection cannot confirm or publish a context row start")
        observe(12, other, playlist.uri)
        try check(model.isLoading && !media.isPublishing, "Another song in the right collection cannot confirm or publish a context row start")
        observe(13, item, playlist.uri, device: "external-output")
        try check(model.isLoading && !media.isPublishing, "The exact context and item on an external device cannot confirm a request for the owned output")
        observe(14, item, playlist.uri, active: false)
        try check(model.isLoading && !media.isPublishing, "An inactive target cannot confirm a context row start")
        observe(15, item, playlist.uri)
        try check(!model.isLoading && media.isPublishing && model.errorMessage == nil, "The fresh exact track, collection and active device confirm a contextual row start")

        let albumQuery = showPage(album, items: [item, other], positions: [0, 1])
        let beforeStale = controls().count
        model.playSpotifyCatalogItem(item, in: playlist, queryId: playlistQuery, index: 1)
        try check(controls().count == beforeStale, "A stale playlist row cannot send a start after switching to an album page")
        model.playSpotifyCatalogItem(other, in: album, queryId: albumQuery, index: 1)
        guard case .startContextItem(let albumTrack, let albumURI, _, _, let albumRow)? = controls().last else {
            throw Failure(message: "The current album row did not send a contextual item start")
        }
        try check(albumTrack.id == other.id && albumURI == album.uri && albumRow.position == 1, "Album row Play also preserves the requested collection and position")
        try await wait { !model.isLoading && model.errorMessage != nil }
        observe(16, other, playlist.uri)
        try check(model.errorMessage != nil && !media.isPublishing, "A late track match in the wrong context keeps the timeout and system session unclaimed")
        observe(17, other, album.uri)
        try check(model.errorMessage == nil && media.isPublishing, "A late exact context match clears its timeout and may publish the owned session")

        model.playSpotifyCollection(album)
        guard case .startContext(let headerURI, _, _)? = controls().last else { throw Failure(message: "Header Play lost its context-only command") }
        try check(headerURI == album.uri, "Collection header Play keeps its context-only command")
        model.playSpotifyCatalogItem(item, in: nil, queryId: albumQuery, index: 0)
        guard case .startOnDevice(let standalone, _, _)? = controls().last else { throw Failure(message: "A standalone row lost its direct play command") }
        try check(standalone.id == item.id, "A catalog song without an album or playlist parent remains a standalone start")

        model.playSpotifyCatalogItem(item, in: album, queryId: albumQuery, index: 0)
        model.playSpotifyCatalogItem(other, in: album, queryId: albumQuery, index: 1)
        try check(model.canTogglePlayback && model.playbackToggleShowsPause, "Pause stays available while the latest album row opens")
        model.togglePlayback()
        try check(model.canTogglePlayback && !model.playbackToggleShowsPause && pauses() == 0,
                  "Pause immediately sets the user's intent without interrupting the earlier track")
        observe(18, item, album.uri, ownedControl: true)
        observe(19, other, playlist.uri, ownedControl: true)
        observe(20, other, album.uri, device: "external-output", ownedControl: true)
        try check(model.isLoading && pauses() == 0, "An earlier click, wrong context or changed device cannot release the queued pause")
        observe(21, other, album.uri, ownedControl: true, pauseCapability: false)
        observe(22, other, album.uri, ownedControl: true)
        try check(!model.isLoading && !model.playbackToggleShowsPause && pauses() == 1
                  && !connection.recorded().contains { if case .pause = $0 { return true }; return false },
                  "The confirmed selection pauses its exact device once despite an interim missing Pause capability, retaining the button intent")

        model.playSpotifyCatalogItem(item, in: album, queryId: albumQuery, index: 0)
        model.togglePlayback(); model.togglePlayback()
        observe(23, item, album.uri, ownedControl: true)
        try check(!model.isLoading && model.isPlaying && pauses() == 1,
                  "A second toggle during loading clears the queued pause")
        model.playSpotifyCatalogItem(other, in: album, queryId: albumQuery, index: 1)
        model.togglePlayback()
        model.playSpotifyCatalogItem(item, in: album, queryId: albumQuery, index: 0)
        observe(24, item, album.uri, ownedControl: true)
        try check(!model.isLoading && pauses() == 1, "A later selection drops the earlier selection's pause")
        model.playSpotifyCatalogItem(other, in: album, queryId: albumQuery, index: 1)
        model.togglePlayback()
        try await wait { !model.isLoading && model.errorMessage != nil }
        observe(25, other, album.uri, ownedControl: true)
        try check(pauses() == 2, "A fresh exact selection within the fixed grace preserves its requested Pause after the visible timeout")

        observe(26, other, album.uri, restored: RestoredRemote(contextUri: album.uri))
        model.playSpotifyCatalogItem(item, in: album, queryId: albumQuery, index: 0)
        try check(model.canTogglePlayback && model.playbackToggleShowsPause,
                  "An explicit choice made over a saved session uses the same loading controls as other selections")
        model.togglePlayback()
        observe(27, item, album.uri, ownedControl: true)
        try check(pauses() == 3, "A new selection can be paused even when it replaced a saved session")

        let externalPause = try JSONSerialization.jsonObject(with: encoder.encode(RemoteControl.pauseOnDevice("external-output"))) as! [String: Any]
        try check(externalPause["kind"] as? String == "pause_on_device" && externalPause["device_id"] as? String == "external-output",
                  "Pausing a selected Connect device encodes its exact destination")
        let scopedPause = try JSONSerialization.jsonObject(with: encoder.encode(
            PlayerCommand.pauseConfirmedRestoredResume(operationId: 41, deviceId: "owned-output"))) as! [String: Any]
        try check(scopedPause["type"] as? String == "pause_confirmed_restored_resume"
                  && scopedPause["operation_id"] as? UInt64 == 41 && scopedPause["device_id"] as? String == "owned-output",
                  "The scoped Pause wire carries the exact Resume operation and device")
        await model.prepareToQuit()
    }
    func ownedSeekPreviousChecks(_ item: MediaItem) async throws {
        let process = FixtureProcess(clockSupported: true, controlSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item]); driver = connection; return connection
        }, playbackRuntime: runtime)
        let connection = driver!
        func projectOwned(_ position: UInt64, previous: Bool = true) {
            var state = frame(item, playing: false, position: position, ownedOutputObserved: true, previous: previous)
            state.ownedOutputControl = true
            project(connection, state, acknowledge: false)
        }
        func clock(_ sequence: UInt64, _ position: UInt64, epoch: UInt64 = 1) async throws {
            await process.clock(item.id, sequence: sequence, state: "paused", rate: 0, position: position, queueEpoch: epoch)
            try await wait { runtime.clock?.sequence == sequence }
        }
        func sentPrevious() -> Bool {
            if case .skipPrevious? = connection.recorded().last { return true }
            return false
        }
        func sentRestart() -> Bool {
            if case .seek(positionMs: 0)? = connection.recorded().last { return true }
            return false
        }

        try await clock(1, 18_500)
        projectOwned(18_500)
        model.seekRelative(seconds: -15)
        try await wait { model.lastCommandOutcome?.requestId == model.latestPositionRequest }
        try await clock(2, 18_500)
        projectOwned(18_500)
        model.previous()
        try check(sentPrevious(), "Previous below four seconds follows the accepted seek target through stale owned clock and snapshots")
        projectOwned(3_500)
        try check(model.isChangingTrack && !model.canSeek,
                  "The snapshot for the preceding seek does not complete Previous")

        try await clock(3, 2_000)
        projectOwned(2_000, previous: false)
        try check(!model.canGoPrevious, "A confirmed position under four seconds requires a previous track")
        model.seek(milliseconds: 4_000)
        try check(model.canGoPrevious, "A seek to exactly four seconds enables restarting even without a previous track")
        model.previous()
        try check(sentRestart(), "Previous at four seconds restarts the track before the seek is confirmed")

        try await clock(4, 18_500)
        projectOwned(18_500)
        let replaced = model.seek(milliseconds: 20_000)!
        model.seek(milliseconds: 3_500)
        connection.receive(.commandResult(.init(requestId: replaced, error: .init(code: "busy", message: "Original stale seek rejection"))))
        model.previous()
        try check(sentPrevious(), "A rejected superseded seek does not discard the latest seek target")

        projectOwned(0)
        projectOwned(18_500)
        let rejected = model.seek(milliseconds: 3_500)!
        connection.receive(.commandResult(.init(requestId: rejected, error: .init(code: "busy", message: "Original current seek rejection"))))
        model.previous()
        try check(sentRestart(), "Rejecting the current seek restores the confirmed Previous interpretation")

        projectOwned(18_500)
        model.seek(milliseconds: 3_500)
        try await clock(5, 18_500, epoch: 2)
        projectOwned(18_500)
        model.previous()
        try check(sentRestart(), "A new occurrence of the same URI drops its predecessor's seek target")

        model.seek(milliseconds: 3_500)
        model.previous()
        var compound = RemoteOutputCommand(id: 170, generation: runtime.clockGeneration, sequence: 5,
            expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900, deviceId: "owned-output",
            mediaId: item.id, control: .init(kind: .previousFromStart, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
        compound.requiredQueueEpoch = 2
        connection.receive(.remoteOutputCommand("spotify", compound))
        projectOwned(0)
        try check(model.isChangingTrack && !model.canSeek,
                  "The native cue to zero stays behind the pending Previous artwork and seek fence")
        let predecessor = track("q", title: "Original previous track")
        await process.clock(predecessor.id, sequence: 6, state: "paused", rate: 0, position: 0, queueEpoch: 3)
        try await wait { runtime.clock?.sequence == 6 }
        project(connection, frame(predecessor, playing: false, position: 0, ownedOutputObserved: true), acknowledge: false)
        try check(model.current?.id == predecessor.id && !model.isChangingTrack && model.canSeek,
                  "The confirmed predecessor completes the native transition and restores scrubbing")

        let context = "spotify:album:" + String(repeating: "c", count: 22)
        let following = track("r", title: "Original context follower")
        func projectRoundTrip(_ current: MediaItem, previous: [MediaItem], next: MediaItem, position: UInt64,
                              playing: Bool = false, allowsControl: Bool = true) {
            var state = frame(current, playing: playing, context: context, position: position,
                              ownedOutputObserved: true, previous: !previous.isEmpty)
            state.ownedOutputControl = allowsControl
            project(connection, state, acknowledge: false,
                    deviceQueue: .init(previous: previous, previousSources: previous.map { _ in .context }, upcomingSources: [.context]),
                    upcoming: [next])
        }
        func acknowledgeScopedNavigation(id: UInt64, kind: RemoteOutputCommand.Control.Kind, source: MediaItem,
                                         sequence: UInt64, epoch: UInt64, request: UInt64? = nil) async throws {
            let command = RemoteOutputCommand(id: id, generation: runtime.clockGeneration, sequence: sequence,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900, requestId: request,
                requiredQueueEpoch: epoch, deviceId: "owned-output", mediaId: source.id,
                control: .init(kind: kind, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
            connection.receive(.remoteOutputCommand("spotify", command))
            try await wait {
                connection.recorded().contains { command in
                    guard case .remote(_, .outputCommandResult(let result)) = command else { return false }
                    return result.id == id && result.accepted
                }
            }
        }

        try await clock(7, 2_500, epoch: 4)
        projectRoundTrip(item, previous: [predecessor], next: following, position: 2_500)
        model.previous()
        try check(sentPrevious() && model.displayedTrack?.id == predecessor.id,
                  "Owned Previous before four seconds initially previews the known predecessor")
        model.next()
        let returnRequest = model.latestPositionRequest
        try await acknowledgeScopedNavigation(id: 171, kind: .previousFromStart, source: item, sequence: 7, epoch: 4)
        try await clock(8, 0, epoch: 4)
        projectRoundTrip(item, previous: [predecessor], next: following, position: 0)
        try check(model.displayedTrack?.id == item.id && model.isChangingTrack && !model.canSeek,
                  "The cue to zero on the original occurrence does not complete the owned Next return")
        await process.clock(predecessor.id, sequence: 9, state: "playing", rate: 1, position: 0, queueEpoch: 5)
        try await wait { runtime.clock?.sequence == 9 }
        projectRoundTrip(predecessor, previous: [], next: item, position: 0, playing: true, allowsControl: false)
        try check(model.current?.id == predecessor.id && model.displayedTrack?.id == item.id && model.isChangingTrack,
                  "The owned Previous observation preserves the latest Next cover and title while native control is temporarily unavailable")
        try await acknowledgeScopedNavigation(id: 172, kind: .next, source: predecessor, sequence: 9, epoch: 5, request: returnRequest)
        try await clock(10, 0, epoch: 6)
        projectRoundTrip(item, previous: [predecessor], next: following, position: 0)
        try check(model.current?.id == item.id && model.displayedTrack?.id == item.id && !model.isChangingTrack && model.canSeek,
                  "The exact return completes the owned Next preview and restores scrubbing")

        model.previous(); model.next()
        guard let rejectedReturn = model.latestPositionRequest else { throw Failure(message: "Missing owned return request") }
        await process.clock(predecessor.id, sequence: 11, state: "playing", rate: 1, position: 0, queueEpoch: 7)
        try await wait { runtime.clock?.sequence == 11 }
        projectRoundTrip(predecessor, previous: [], next: item, position: 0, playing: true, allowsControl: false)
        try check(model.current?.id == predecessor.id && model.displayedTrack?.id == item.id && model.isChangingTrack,
                  "A second Previous and Next preserves the current selection during the intermediate observation")
        connection.receive(.commandResult(.init(requestId: rejectedReturn,
            error: .init(code: "unavailable", message: "Original relative Next rejection"))))
        try check(model.displayedTrack?.id == predecessor.id && !model.isChangingTrack,
                  "When the return is rejected, the actual predecessor replaces the pending Next preview")

        await model.prepareToQuit()
    }

    func ownedStartChecks(_ item: MediaItem) async throws {
        let process = FixtureProcess(clockSupported: true, controlSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item]); driver = connection; return connection
        }, playbackRuntime: runtime)
        let connection = driver!
        let base = frame(item)
        func owned(_ observation: UInt64, receipt: UInt64? = nil, issue: PlayerIssue? = nil, observed: RemoteState? = nil) -> RemoteState {
            RemoteState(controlResult: receipt.map { RemoteControlResult(operationId: $0, error: issue) },
                        observationId: observation, ownedOutputObserved: true, ownedOutputControl: true,
                        provider: "spotify", connection: .unavailable, authenticationRequired: false,
                        issue: PlayerIssue(code: "rate_limited", message: "Original catalog quota"),
                        retryAfterSeconds: 30, devices: base.devices, playback: observed?.playback ?? base.playback)
        }
        project(connection, owned(7), acknowledge: false)
        await process.clock(item.id, sequence: 1)
        try await wait { runtime.clock?.sequence == 1 }
        model.startSpotifyTrack(item)
        let operation = connection.recorded().compactMap { command -> UInt64? in
            if case .remote(_, .control(let control)) = command { return control.operationId }
            return nil
        }.last!
        func sendLocal(_ id: UInt64, operation: UInt64, uri: String? = nil) {
            let control = RemoteOutputCommand.Control(kind: .startUri, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: uri ?? item.id, operationId: operation)
            connection.receive(.remoteOutputCommand("spotify", RemoteOutputCommand(id: id, generation: runtime.clockGeneration,
                sequence: 1, expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900, deviceId: "owned-output", mediaId: item.id, control: control)))
        }
        func receipt(_ id: UInt64, accepted: Bool) -> Bool {
            connection.recorded().contains { command in
                if case .remote(_, .outputCommandResult(let result)) = command { return result.id == id && result.accepted == accepted }
                return false
            }
        }
        sendLocal(1, operation: operation + 1)
        try check(receipt(1, accepted: false), "A superseded local start cannot reach the supervisor")
        sendLocal(2, operation: operation)
        try await wait { receipt(2, accepted: true) }
        project(connection, owned(7, receipt: operation), acknowledge: false)
        try check(model.isLoading && model.errorMessage == nil, "A local receipt under HTTP quota cannot confirm the pre-dispatch observation")
        project(connection, owned(8, receipt: operation), acknowledge: false)
        try check(!model.isLoading && model.isPlaying && model.errorMessage == nil, "Fresh owned playback confirms the local start without a connected Web API")
        let uncertain = PlayerIssue(code: "provider_unavailable", message: "Original lost local receipt")
        model.startSpotifyTrack(item)
        sendLocal(3, operation: operation + 1)
        try await wait { receipt(3, accepted: true) }
        project(connection, owned(9, receipt: operation + 1, issue: uncertain), acknowledge: false)
        try check(model.isLoading && model.errorMessage == nil,
                  "A lost receipt and new clock tick cannot confirm replaying an already-playing selection")
        let context = RemoteCollection(id: String(repeating: "p", count: 22), title: "Original playlist",
                                       url: "https://open.spotify.com/playlist/", artwork: nil, kind: .playlist)
        model.playSpotifyCollection(context)
        sendLocal(4, operation: operation + 2, uri: context.uri)
        try await wait { receipt(4, accepted: true) }
        project(connection, owned(9, receipt: operation + 2, issue: uncertain), acknowledge: false)
        try check(model.isLoading && model.errorMessage == nil,
                  "The exact lost local start receipt remains pending while playback is still on the old selection")
        let playlistTrack = track("r", title: "Original playlist selection")
        project(connection, owned(10, observed: frame(playlistTrack, context: context.uri)), acknowledge: false)
        try check(!model.isLoading && model.errorMessage == nil && model.current?.id == playlistTrack.id,
                  "Fresh playback on the requested owned context confirms a start whose receipt was lost")
        await model.prepareToQuit()
    }
    func lateSelectionPauseChecks(_ item: MediaItem) async throws {
        let target = track("t", title: "Original delayed selection")
        let next = track("n", title: "Original superseding selection")
        for scenario in ["confirmed", "expired", "new selection", "runtime", "account", "logout", "bridge"] {
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var driver: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive, items: [item, target, next], details: .manual)
                driver = connection; return connection
            }, playbackRuntime: runtime, remoteConfirmationTimeout: .milliseconds(40),
                remotePauseConfirmationGrace: scenario == "expired" ? .milliseconds(120) : .seconds(2))
            let connection = driver!
            func supplyProfile(_ id: String) throws {
                model.inspectSpotify(.profile, refresh: true)
                guard let query = model.spotifyDetailQueries.values.first(where: { $0.kind == .profile }) else {
                    throw Failure(message: "Missing late-Pause account query")
                }
                connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: query.queryId,
                    offset: 0, total: 1, items: [], collections: [RemoteCollection(id: id, title: id,
                        url: "", artwork: nil, kind: .profile)], issue: nil)))
            }
            func owned(_ media: MediaItem, observation: UInt64, operation: UInt64? = nil,
                       device: String = "owned-output") -> RemoteState {
                var state = frame(media, device: device, ownedOutputObserved: true)
                state.ownedOutputControl = true; state.observationId = observation
                if let operation {
                    state.controlResult = RemoteControlResult(operationId: operation,
                        error: PlayerIssue(code: "provider_unavailable", message: "Original ambiguous local start"))
                }
                return state
            }
            func pauses(_ source: FixtureConnection) -> [String] {
                source.recorded().compactMap {
                    if case .remote("spotify", .control(.pauseOnDevice(let device))) = $0 { return device }
                    return nil
                }
            }
            project(connection, owned(item, observation: 10), acknowledge: false)
            try supplyProfile("original-listener")
            await process.clock(item.id, sequence: 1, rate: 1, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            model.startSpotifyTrack(target); model.togglePlayback()
            guard let operation = connection.recorded().compactMap({ command -> UInt64? in
                if case .remote(_, .control(.startOnDevice(_, _, let operation))) = command { return operation }
                return nil
            }).last else { throw Failure(message: "Missing delayed selection operation") }
            await process.omitNextControlResult()
            connection.receive(.remoteOutputCommand("spotify", RemoteOutputCommand(id: 90,
                generation: runtime.clockGeneration, sequence: 1,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900,
                deviceId: "owned-output", mediaId: item.id,
                control: .init(kind: .startUri, positionMs: nil, percent: nil, enabled: nil, mode: nil,
                    uri: target.id, operationId: operation))))
            project(connection, owned(item, observation: 11, operation: operation), acknowledge: false)
            try await wait { !model.isLoading && model.errorMessage?.contains("not been confirmed") == true }
            try check(pauses(connection).isEmpty, "An ambiguous Start timeout cannot pause the previous track")

            if scenario == "expired" {
                // Replayed failure/old-track observations must not extend the deadline.
                for observation in UInt64(12)...14 {
                    try await Task.sleep(for: .milliseconds(70))
                    project(connection, owned(item, observation: observation, operation: operation), acknowledge: false)
                }
            } else if scenario == "new selection" {
                model.startSpotifyTrack(next)
            } else if scenario == "runtime" {
                let previous = runtime.clockGeneration
                runtime.retry(); try await wait { runtime.clockGeneration != previous && runtime.status.phase == .ready }
            } else if scenario == "account" {
                try supplyProfile("different-listener")
            } else if scenario == "logout" {
                model.disconnectSpotify()
            } else if scenario == "bridge" {
                await model.restartPlayer()
            }

            if scenario == "confirmed" {
                project(connection, owned(target, observation: 15, operation: operation, device: "external-output"), acknowledge: false)
                project(connection, owned(item, observation: 16, operation: operation), acknowledge: false)
                try check(pauses(connection).isEmpty, "A late external target or old track cannot release the saved Pause")
            }
            let finalDriver = driver!
            project(finalDriver, owned(target, observation: 17, operation: operation), acknowledge: false)
            project(finalDriver, owned(target, observation: 18, operation: operation), acknowledge: false)
            if scenario == "confirmed" {
                try check(pauses(connection) == ["owned-output"] && !model.playbackToggleShowsPause
                          && !connection.recorded().contains {
                              if case .pause = $0 { return true }
                              if case .pauseConfirmedRestoredResume = $0 { return true }
                              return false
                          }, "A delayed ambiguous owned selection dispatches its original device Pause exactly once after timeout")
            } else {
                try check(pauses(connection).isEmpty && pauses(finalDriver).isEmpty,
                          "A \(scenario) late selection cannot reuse the expired or superseded Pause authority")
            }
            await model.prepareToQuit()
        }
    }
    func chapterOutputChecks() async throws {
        let savedLighting = UserDefaults.standard.object(forKey: "AuvraReactiveLighting")
        defer { if let savedLighting { UserDefaults.standard.set(savedLighting, forKey: "AuvraReactiveLighting") } else { UserDefaults.standard.removeObject(forKey: "AuvraReactiveLighting") } }
        let id = String(repeating: "c", count: 22), canonical = "spotify:episode:" + String(repeating: "c", count: 22)
        let chapter = MediaItem(id: "spotify:chapter:" + id, title: "Original playable chapter", artists: ["Original book author"],
                                album: "Original book", durationMs: 120000, artwork: nil, capabilities: capabilities)
        try check(SpotifyPlaybackIdentity.matches(chapter.id, canonical) && SpotifyPlaybackIdentity.matches(canonical, chapter.id),
                  "Chapter identity matches only its exact episode playback alias in either direction")
        for other in ["spotify:episode:" + String(repeating: "d", count: 22), "spotify:track:" + id, "other:episode:" + id,
                      "spotify:episode:short", "spotify:episode:" + id + ":extra"] {
            try check(!SpotifyPlaybackIdentity.matches(chapter.id, other), "A chapter playback alias cannot match another record or namespace")
        }
        try check(!SpotifyPlaybackIdentity.matches(nil, nil) && !SpotifyPlaybackIdentity.matches("spotify:chapter:short", "spotify:episode:short"),
                  "Missing and malformed aliases cannot acquire playback identity")
        let process = FixtureProcess(clockSupported: true, controlSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [chapter]); driver = connection; return connection
        }, playbackRuntime: runtime)
        let connection = driver!, media = MediaPlayerSystemSession(player: model); model.attachSystemMedia(media)
        await process.clock(canonical, sequence: 1, state: "paused", rate: 0)
        try await wait { runtime.clock?.sequence == 1 }
        project(connection, frame(chapter, playing: false))
        try check(!media.isPublishing, "A paused chapter restore with an episode clock cannot acquire the macOS session")
        model.startSpotifyTrack(chapter)
        await process.clock(canonical, sequence: 2, position: 500)
        try await wait { runtime.clock?.sequence == 2 }
        let observed = MediaItem(id: canonical, title: chapter.title, artists: chapter.artists, album: chapter.album,
                                 durationMs: chapter.durationMs, artwork: nil, capabilities: chapter.capabilities)
        project(connection, frame(observed, position: 500))
        try check(!model.isLoading && model.errorMessage == nil && model.isPlaying,
                  "Fresh episode playback confirms the selected chapter without an endless loading state")
        project(connection, frame(chapter, position: 500, ownedOutputObserved: true))
        try check(media.isPublishing && (MPNowPlayingInfoCenter.default().nowPlayingInfo?[MPMediaItemPropertyArtist] as? String) == chapter.artist,
                  "An authenticated owned chapter with an episode clock publishes its real author to macOS")
        try await Task.sleep(for: .milliseconds(380))
        await process.clock(canonical, sequence: 3, position: 1100)
        try await wait { runtime.clock?.sequence == 3 }
        model.setReactiveLighting(true); model.setAnalysisVisible(true)
        try check(connection.recorded().contains { if case .hostAnalysis(let lease) = $0 { return lease?.mediaId == chapter.id && lease?.deviceId == "owned-output" && lease?.processId == 12345 }; return false },
                  "The exact chapter alias retains the logical media ID in its owned process analysis lease")
        await process.clock("spotify:episode:" + String(repeating: "d", count: 22), sequence: 4)
        try await wait { runtime.clock?.sequence == 4 }
        project(connection, frame(chapter, ownedOutputObserved: true))
        try check(!media.isPublishing, "An episode clock for a different chapter cannot publish retained chapter metadata")
        try check(connection.recorded().contains { if case .hostAnalysis(nil) = $0 { return true }; return false },
                  "A different chapter clock clears the previous process analysis lease")
        model.transferSpotify("external-output")
        await process.clock(canonical, sequence: 5)
        try await wait { runtime.clock?.sequence == 5 }
        project(connection, frame(chapter, device: "external-output"))
        try check(!media.isPublishing, "A matching chapter alias cannot grant an external Connect device the macOS session")
        await model.prepareToQuit()
    }
    func outputClockChecks(_ item: MediaItem) async throws {
        let process = FixtureProcess(clockSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let value = FixtureConnection(receive,items: [item],details: .manual); driver = value; return value
        },playbackRuntime: runtime)
        let connection = driver!; let media = MediaPlayerSystemSession(player: model); model.attachSystemMedia(media)
        project(connection, frame(item))
        try check(!media.isPublishing, "A clock-capable restarted child cannot claim Now Playing from a cached HTTP Playing frame")
        await process.clock(item.id, sequence: 1)
        try await wait { runtime.clock?.sequence == 1 }
        let observations = connection.recorded().compactMap { command -> RemoteOutputClock? in if case .remote(_, .outputClock(let value)) = command { return value }; return nil }
        try check(observations.count == 1 && observations[0].mediaId == item.id && observations[0].deviceId == "owned-output" && observations[0].rate == 1.5, "Only the authenticated owned output clock crosses the Store and command boundary")
        let userRequest = model.send(.setVolume(0.2))
        try await wait { model.lastCommandOutcome?.requestId == userRequest }
        let clockRequest = model.send(.remote(provider: "spotify", action: .outputClock(observations[0])), outputUpdate: true)
        let leaseRequest = model.send(.hostAnalysis(nil), outputUpdate: true)
        try check(!model.pendingCommands.contains(clockRequest) && !model.pendingCommands.contains(leaseRequest),
                  "Output heartbeats do not invalidate the pending user commands viewed by lyrics")
        await withCheckedContinuation { (completed: CheckedContinuation<Void, Never>) in
            DispatchQueue.main.async { completed.resume() }
        }
        try check(model.lastCommandOutcome?.requestId == userRequest,
                  "Successful output heartbeats leave the last user command receipt available")
        try check(!connection.recorded().contains { if case .remote(_, .adoptOutput) = $0 { return true }; return false }, "Initial projection waits for the account before adopting a live owned output")
        let profileQuery = model.spotifyDetailQueries.values.first { $0.kind == .profile }!
        let profile = RemoteCollection(id: "original-listener", title: "Original listener", url: "https://open.spotify.com/", artwork: nil, kind: .profile)
        connection.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: profileQuery.queryId, offset: 0,
                                                  total: 1, items: [], collections: [profile], issue: nil)))
        try await wait { model.spotifyProfileID != nil && connection.recorded().contains { if case .remote(_, .adoptOutput) = $0 { return true }; return false } }
        // Project the coordinator's confirmed rate through the actual snapshot DTO.
        var rated = frame(item, position: 2000)
        rated = RemoteState(provider: rated.provider,connection: rated.connection,authenticationRequired: false,issue: nil,retryAfterSeconds: nil,devices: rated.devices,playback: RemotePlayback(positionRate: 1.5, state: .playing,positionMs: 2000,volume: 0.2,current: item,upcoming: [],shuffle: false,repeat: .off,capabilities: capabilities,transport: rated.playback!.transport,device: rated.playback?.device))
        project(connection, rated)
        try check(media.isPublishing && (MPNowPlayingInfoCenter.default().nowPlayingInfo?[MPNowPlayingInfoPropertyPlaybackRate] as? Double) == 1.5, "The owned speed reaches the real macOS MediaPlayer metadata")
        let before = model.position(), at = ProcessInfo.processInfo.systemUptime
        try await Task.sleep(for: .milliseconds(120))
        try check(abs(model.position() - before - (ProcessInfo.processInfo.systemUptime - at) * 1.5) < 0.05, "The shared UI and lyrics position advance at the confirmed playback speed")
        await process.clock(item.id, sequence: 2, active: false)
        try await wait { runtime.status.active == false }
        project(connection, rated)
        try check(!media.isPublishing, "A local device-loss event clears the system lease despite cached HTTP Playing")
        await process.clock(item.id, sequence: 3, state: "paused", rate: 0)
        try await wait { runtime.clock?.sequence == 3 }
        try check(!media.isPublishing, "Returning an owned output while paused cannot reacquire a lost media lease")
        await process.clock(item.id, sequence: 4)
        try await wait { runtime.clock?.sequence == 4 }
        project(connection, rated)
        try check(media.isPublishing, "Actual Playing on the returned owned output reacquires the system session")
        model.transferSpotify("external")
        await process.clock(item.id, sequence: 5)
        try await wait { runtime.clock?.sequence == 5 }
        try check(!media.isPublishing && !connection.recorded().contains { if case .remote(_, .outputClock(let clock)) = $0 { return clock?.sequence == 5 }; return false }, "Selecting an external output prevents further owned clocks from crossing the bridge")
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let wire = try JSONSerialization.jsonObject(with: encoder.encode(RemoteAction.outputClock(observations[0]))) as! [String: Any]
        try check(wire["kind"] as? String == "output_clock" && (wire["clock"] as? [String: Any])?["device_id"] as? String == "owned-output", "The compact output observation uses the real snake-case action wire format")
        await model.prepareToQuit()
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var restoredDriver: FixtureConnection?
        let restored = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let value = FixtureConnection(receive,items: [item]); restoredDriver = value; return value
        },playbackRuntime: runtime)
        let saved = PlaybackSnapshot(restoredRemote: RestoredRemote(contextUri: nil),revision: 1,state: .paused,positionMs: 42000,volume: 0.2,activeProvider: "spotify",providerConnection: .connected,capabilities: capabilities,queue: QueueState(revision: 1,context: .userQueue,current: item,upcoming: [],history: [],repeatMode: .off,shuffle: false,playNextCount: 0,forwardCount: 0),transport: .local,playbackSessionActive: false,session: SessionState(enabled: false,ready: true,restored: true,saving: false,issue: nil),queueIssues: [])
        restoredDriver!.receive(.snapshot(saved))
        restored.acceptSpotifyAccess("original-profile-fixture")
        try await wait { restored.spotifyProfileID != nil }
        await process.clock(item.id,sequence: 1,state: "paused",rate: 0)
        try await wait { runtime.clock?.state == .paused }
        restoredDriver!.receive(.remoteState(frame(item)))
        try check(restored.snapshot?.restoredRemote != nil && restored.position() == 42 && !restoredDriver!.recorded().contains { if case .remote(_, .adoptOutput) = $0 { return true }; return false }, "A confirmed paused owned clock prevents cached HTTP Playing from bypassing paused restore")
        let other = track("r", title: "Original unrelated session")
        await process.clock(other.id, sequence: 2)
        try await wait { runtime.clock?.sequence == 2 }
        restoredDriver!.receive(.remoteState(frame(other, position: 18_000)))
        try check(restored.current?.id == item.id && !restored.isPlaying && restored.position() == 42
                  && !restoredDriver!.recorded().contains { if case .remote(_, .adoptOutput) = $0 { return true }; return false },
                  "Live playback of a different track on the owned output cannot discard paused restore")
        restored.togglePlayback()
        try await wait { restoredDriver!.recorded().contains { command in
            if case .remote(_, .control(.resumeOnDevice(let selected, let device, let position, _, _))) = command {
                return selected.id == item.id && device == "owned-output" && position == 42_000
            }
            return false
        } }
        await restored.prepareToQuit()
    }
    func outputOptionsChecks(_ item: MediaItem) async throws {
        let process = FixtureProcess(clockSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(),owner: process,deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in let value = FixtureConnection(receive,items: [item]); driver = value; return value },playbackRuntime: runtime)
        let connection = driver!; let media = MediaPlayerSystemSession(player: model); model.attachSystemMedia(media)
        project(connection,frame(item)); await process.clock(item.id,sequence: 1); try await wait { runtime.clock?.sequence == 1 }
        func advertised(queueAdd: Bool, enhancement: PlaybackEnhancement? = .recommendations) -> RemoteState {
            let device = frame(item).playback!.device
            let controls = TransportControls(queueAdd: queueAdd,seekForwardMs: 15000,seekBackwardMs: 30000,stop: false,next: false,previous: false,shuffle: true,repeat: true,queueEdit: false)
            let capabilities = MediaCapabilities(play: true,pause: false,seek: true,volume: true,pcmAnalysis: false,systemMediaSession: false)
            return RemoteState(provider: "spotify",connection: .connected,authenticationRequired: false,issue: nil,retryAfterSeconds: nil,devices: [device!],playback: RemotePlayback(enhancement: enhancement,positionRate: 0,state: .paused,positionMs: 2000,volume: 0.37,current: item,upcoming: [],shuffle: true,repeat: .all,capabilities: capabilities,transport: controls,device: device))
        }
        project(connection,advertised(queueAdd: false))
        try check(model.canTogglePlayback && !model.canGoNext && !model.canGoPrevious && model.canShuffle && model.canRepeat, "Owned action availability reaches transport without enabling unavailable track skips")
        try check(abs(model.volume - 0.37) < 0.001 && model.snapshot?.queue.shuffle == true && model.snapshot?.queue.repeatMode == .all && model.snapshot?.enhancement == .recommendations, "Owned volume and modes reach the shared Store and queue projection")
        try check(model.skipForwardSeconds == 15 && model.skipBackwardSeconds == 30 && MPRemoteCommandCenter.shared().skipForwardCommand.preferredIntervals.first?.doubleValue == 15 && MPRemoteCommandCenter.shared().skipBackwardCommand.preferredIntervals.first?.doubleValue == 30, "Advertised relative steps reach real MediaPlayer preferred intervals")
        model.enqueueSpotifyTrack(item)
        try check(!connection.recorded().contains { if case .remote(_, .control(.enqueue)) = $0 { return true }; return false }, "Queue-add denial is rechecked before submitting a command")
        project(connection,advertised(queueAdd: true))
        let episode = MediaItem(id: "spotify:episode:" + String(repeating: "e",count: 22),title: "Original episode",artists: item.artists,album: nil,durationMs: 100000,artwork: nil,capabilities: capabilities)
        let chapter = MediaItem(id: "spotify:chapter:" + String(repeating: "c",count: 22),title: "Original chapter",artists: item.artists,album: nil,durationMs: 100000,artwork: nil,capabilities: capabilities)
        try check(model.canEnqueueSpotifyTrack(item) && model.canEnqueueSpotifyTrack(episode) && !model.canEnqueueSpotifyTrack(chapter), "Queue-add supports songs and episodes while chapters remain unavailable")
        model.enqueueSpotifyTrack(item)
        try check(connection.recorded().contains { if case .remote(_, .control(.enqueue(let selected))) = $0 { return selected.id == item.id }; return false }, "An allowed queue-add reaches the existing transport command path")
        let queuedSong = track("q", title: "Original queued song")
        project(connection,advertised(queueAdd: true),
                deviceQueue: DeviceQueueDetails(previous: [item],previousSources: [.autoplay],upcomingSources: [.queue,.autoplay]),
                upcoming: [queuedSong,episode])
        try check(!model.canEditQueue && model.snapshot?.queue.deviceQueue?.previous.first?.id == item.id
                  && model.snapshot?.queue.deviceQueue?.previousSources == [.autoplay]
                  && model.snapshot?.queue.deviceQueue?.upcomingSources == [.queue,.autoplay], "Device queue renders history and provenance without local edit actions")
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let legacy = try decoder.decode(DeviceQueueDetails.self,from: Data(#"{"previous":[],"upcoming_sources":[]}"#.utf8))
        let misaligned = try decoder.decode(DeviceQueueDetails.self,from: Data(#"{"previous":[],"previous_sources":["autoplay",null],"upcoming_sources":[]}"#.utf8))
        try check(legacy.previousSources == nil
                  && misaligned.previousSources == [.autoplay,nil]
                  && QueueView.sourceLabel(in: misaligned.previousSources,rowCount: 1,at: 0) == nil
                  && QueueView.sourceLabel(in: [.autoplay,.queue],rowCount: 2,at: 0) == "Autoplay"
                  && QueueView.sourceLabel(in: [.autoplay,.queue],rowCount: 2,at: 1) == "Queued", "Queue provenance requires a complete index-aligned array")
        try await render(QueueView(player: model,navigation: AppNavigation()).background(Color(white: 0.08)),name: "queue-recommendations",size: NSSize(width: 960,height: 650))
        try await render(QueueView(player: model,navigation: AppNavigation(),dismissPanel: {}).background(AuvraColors.raised),
                         name: "light-queue-panel",size: NSSize(width: 392,height: 430),colorScheme: .light)
        project(connection,advertised(queueAdd: true,enhancement: nil))
        try check(model.snapshot?.enhancement == nil, "An absent recommendation mode does not leave a stale enhancement badge")
        await model.prepareToQuit()
    }
    func ownedDeviceOmissionChecks(_ item: MediaItem) async throws {
        let context = "spotify:album:" + String(repeating: "c", count: 22)
        for scenario in ["playing", "paused", "loading", "projection expiry", "unplayed", "auth", "runtime", "inactive", "stale", "selected foreign", "restricted", "renamed", "ambiguous", "external active", "foreign current"] {
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let driver = FixtureConnection(receive, items: [item], details: .manual); fixture = driver; return driver
            }, playbackRuntime: runtime, lyricsProvider: OriginalLyricsFixture())
            let driver = fixture!
            var owned = frame(item, playing: scenario != "unplayed", context: context, ownedOutputObserved: true)
            owned.ownedOutputControl = true
            if scenario != "unplayed" { project(driver, owned, acknowledge: false) }
            await process.clock(item.id, sequence: 1, state: scenario == "unplayed" ? "paused" : "playing",
                rate: scenario == "unplayed" ? 0 : 1, queueEpoch: 7, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            if scenario != "unplayed" { try check(model.ownedSpotifyDevice?.id == "owned-output", "An actual owned Playing session has a verified cloud descriptor") }
            let baseline = driver.recorded().count
            var devices: [RemoteDevice] = []
            var omittedPlayback = owned.playback
            var authenticationRequired = false
            var connection: ProviderConnection = .connected
            switch scenario {
            case "paused":
                owned = frame(item, playing: false, context: context, ownedOutputObserved: true)
                omittedPlayback = owned.playback
                await process.clock(item.id, sequence: 2, state: "paused", rate: 0, queueEpoch: 7, context: context, complete: true)
            case "loading":
                await process.clock(track("s").id, sequence: 2, state: "loading", rate: 0, position: 0, queueEpoch: 8)
            case "auth": authenticationRequired = true; connection = .disconnected
            case "runtime":
                runtime.retry(); try await wait { runtime.status.phase == .ready }
                driver.receive(.remoteState(RemoteState(ownedOutputObserved: true, ownedOutputControl: true,
                    provider: "spotify", connection: .connected, authenticationRequired: false,
                    issue: nil, retryAfterSeconds: nil, devices: [], playback: owned.playback)))
                await process.clock(item.id, sequence: 1, rate: 1, queueEpoch: 7, context: context, complete: true)
            case "inactive": await process.clock(item.id, sequence: 2, rate: 1, active: false, queueEpoch: 7, context: context, complete: true)
            case "stale": await process.clock(item.id, sequence: 2, rate: 1, queueEpoch: 7, context: context, complete: true, uptime: ProcessInfo.processInfo.systemUptime - 0.95)
            case "selected foreign": model.transferSpotify("external-output")
            case "restricted": devices = [RemoteDevice(id: "owned-output", name: "Auvra fixture", deviceType: "Computer", volume: 0.2, isActive: true, isRestricted: true)]
            case "renamed": devices = [RemoteDevice(id: "owned-output", name: "Other device", deviceType: "Computer", volume: 0.2, isActive: true, isRestricted: false)]
            case "ambiguous": devices = [RemoteDevice(id: "replacement", name: "Auvra fixture", deviceType: "Computer", volume: 0.2, isActive: false, isRestricted: false), owned.devices[0]]
            case "external active": devices = [RemoteDevice(id: "external-output", name: "External speaker", deviceType: "Speaker", volume: 0.2, isActive: true, isRestricted: false)]
            case "foreign current": omittedPlayback = frame(item, device: "external-output", context: context, ownedOutputObserved: true).playback
            default: break
            }
            if ["paused", "loading", "inactive", "stale"].contains(scenario) { try await wait { runtime.clock?.sequence == 2 } }
            let omitted = RemoteState(ownedOutputObserved: !["loading", "projection expiry"].contains(scenario), ownedOutputControl: true,
                provider: "spotify", connection: connection, authenticationRequired: authenticationRequired,
                issue: nil, retryAfterSeconds: nil, devices: devices, playback: omittedPlayback)
            if scenario == "unplayed" { project(driver, omitted, acknowledge: false, restored: RestoredRemote(contextUri: context)) }
            else { driver.receive(.remoteState(omitted)) }
            let continuation = ["playing", "paused", "loading"].contains(scenario)
            if continuation {
                try check(model.ownedSpotifyDevice?.id == "owned-output" && model.spotifyState?.devices.isEmpty == true,
                          "\(scenario) retains only the proven device identity without fabricating a cloud row")
                let commands = driver.recorded().dropFirst(baseline)
                try check(!commands.contains { if case .remote("spotify", .outputClock(nil)) = $0 { return true }; return false },
                          "\(scenario) device omission does not clear a fresh authenticated owned clock")
                await process.clock(scenario == "loading" ? track("s").id : item.id, sequence: 3,
                    state: scenario == "paused" ? "paused" : "playing",
                    rate: scenario == "paused" ? 0 : 1, queueEpoch: scenario == "loading" ? 8 : 7, context: context, complete: true)
                try await wait { runtime.clock?.sequence == 3 }
                try check(driver.recorded().dropFirst(baseline).contains {
                    if case .remote("spotify", .outputClock(let clock)) = $0 { return clock?.sequence == 3 }; return false
                }, "The next real clock is forwarded while its cloud descriptor remains omitted")
            } else if scenario == "projection expiry" {
                try check(model.ownedSpotifyDevice?.id == "owned-output", "A granular metadata gap briefly keeps the proven device address")
                try await Task.sleep(for: .milliseconds(850))
                await process.clock(item.id, sequence: 2, rate: 1, queueEpoch: 7, context: context, complete: true)
                try await wait { runtime.clock?.sequence == 2 }
                try check(model.ownedSpotifyDevice == nil, "Fresh packets cannot extend an unverified projection gap indefinitely")
            } else {
                // Ambiguous explicit rows keep the original lookup's nil result;
                // other explicit replacement rows may still identify setup data,
                // but none may resurrect the omitted previous descriptor.
                if ["restricted", "renamed", "ambiguous", "external active", "foreign current", "auth", "runtime", "inactive", "stale", "selected foreign", "unplayed"].contains(scenario) {
                    let withoutRows = RemoteState(ownedOutputObserved: true, ownedOutputControl: true,
                        provider: "spotify", connection: connection, authenticationRequired: authenticationRequired,
                        issue: nil, retryAfterSeconds: nil, devices: [], playback: omittedPlayback)
                    driver.receive(.remoteState(withoutRows))
                    try check(model.ownedSpotifyDevice == nil, "\(scenario) revokes the old descriptor instead of using omission to acquire an output")
                }
            }
            await model.prepareToQuit()
        }
    }
    func ownedActivationSelectionChecks(_ item: MediaItem) async throws {
        let context = "spotify:album:" + String(repeating: "c", count: 22)
        let playlist = RemoteCollection(id: String(repeating: "p", count: 22), title: "Original bootstrap playlist", url: "https://open.spotify.com/playlist/", artwork: nil, kind: .playlist)
        for scenario in ["track", "known inactive", "collection", "restored", "profile before ACK", "profile after ACK", "profile after submit", "known account change", "credential after submit", "cancel", "timeout", "active without ACK", "no device", "runtime", "account", "external", "reject", "later track", "later spoken"] {
            let suite = "app.auvra.activation-selection." + UUID().uuidString
            let defaults = UserDefaults(suiteName: suite)!
            defer { defaults.removePersistentDomain(forName: suite) }
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            await process.holdActivationACKs()
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture", volumeDefaults: defaults)
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            let selected = scenario == "later spoken"
                ? MediaItem(id: "spotify:episode:" + String(repeating: "e", count: 22), title: "Original spoken activation fixture", artists: item.artists, album: nil, durationMs: 240_000, artwork: nil, capabilities: capabilities)
                : item
            let target = track("t", title: "Original bootstrap target")
            var fixture: FixtureConnection?
            let shortDeadline = ["timeout", "active without ACK", "no device"].contains(scenario)
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let driver = FixtureConnection(receive, items: [selected, target], details: .manual); fixture = driver; return driver
            }, playbackRuntime: runtime, remoteConfirmationTimeout: shortDeadline ? .milliseconds(280) : .seconds(2),
                lyricsProvider: OriginalLyricsFixture(), volumeDefaults: defaults)
            let driver = fixture!
            let media = MediaPlayerSystemSession(player: model); model.attachSystemMedia(media)
            model.setSpotifySpatialStereo(true)
            driver.receive(.remoteState(RemoteState(provider: "spotify", connection: .connected, authenticationRequired: false, issue: nil, retryAfterSeconds: nil, devices: [], playback: nil)))
            revision += 1
            let restored = !["track", "known inactive", "collection", "active without ACK", "external"].contains(scenario)
            driver.receive(.snapshot(PlaybackSnapshot(restoredRemote: restored ? RestoredRemote(contextUri: context) : nil,
                revision: revision, state: .paused, positionMs: 60_000, volume: 0.2, activeProvider: "spotify", providerConnection: .connected,
                capabilities: .init(play: false, pause: false, seek: false, volume: false, pcmAnalysis: false, systemMediaSession: false),
                queue: QueueState(revision: revision, context: .userQueue, current: selected, upcoming: [], history: [], repeatMode: .off, shuffle: false, playNextCount: 0, forwardCount: 0),
                transport: .init(stop: false, next: false, previous: false, shuffle: false, repeat: false, queueEdit: false), playbackSessionActive: false,
                session: SessionState(enabled: false, ready: true, restored: restored, saving: false, issue: nil), queueIssues: [])))
            await process.clock(selected.id, sequence: 1, state: "paused", rate: 0, active: false, position: 60_000, queueEpoch: 7, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            if scenario == "known inactive" { driver.receive(.remoteState(frame(selected, playing: false, context: context, active: false))) }
            func targets() -> [RemoteControl] {
                driver.recorded().compactMap { command in
                    guard case .remote(_, .control(let control)) = command else { return nil }
                    switch control { case .start, .startOnDevice, .startSpokenOnDevice, .startContext, .startContextItem, .resumeOnDevice: return control; default: return nil }
                }
            }
            func hasCapture() -> Bool { driver.recorded().contains { if case .hostAnalysis(.some) = $0 { return true }; return false } }
            @MainActor func deliverProfile(_ id: String, refresh: Bool = false) throws {
                if refresh { model.inspectSpotify(.profile, refresh: true) }
                guard let query = driver.recorded().compactMap({ command -> RemoteQuery? in
                    if case .remote(_, .query(let query)) = command, query.kind == .profile { return query }; return nil
                }).last else { throw Failure(message: "Missing activation profile query") }
                let profile = RemoteCollection(id: id, title: "Original fixture listener", url: "https://open.spotify.com/user/" + id, artwork: nil, kind: .profile)
                driver.receive(.remotePage(RemotePage(detail: true, provider: "spotify", queryId: query.queryId,
                    offset: 0, total: 1, items: [], collections: [profile], issue: nil)))
            }
            if scenario == "external" {
                model.transferSpotify("external-output")
                project(driver, frame(selected, device: "external-output", playing: false, context: context), acknowledge: true)
                model.startSpotifyTrack(target)
                try check(targets().contains { if case .startOnDevice(_, "external-output", _) = $0 { return true }; return false }, "An explicit external destination keeps its normal Start path")
                try check(await process.activationRequests.isEmpty, "Selecting an external device never activates the owned child")
                await model.prepareToQuit(); continue
            }
            if restored {
                try check(model.snapshot?.capabilities.play == false && model.canPlay && model.canTogglePlayback,
                          "Private owned availability enables explicit Resume without changing coordinator capabilities")
            }
            if ["known account change", "credential after submit"].contains(scenario) { try deliverProfile("original-listener") }
            let started = ContinuousClock.now
            let commandBaseline = driver.recorded().count
            if ["track", "known inactive"].contains(scenario) { model.startSpotifyTrack(target) }
            else if ["collection", "active without ACK"].contains(scenario) { model.playSpotifyCollection(playlist) }
            else { model.togglePlayback() }
            for _ in 0..<100 {
                if await process.activationRequests.count == 1 { break }
                try await Task.sleep(for: .milliseconds(10))
            }
            try check(await process.activationRequests.count == 1, "\(scenario) activates at most once for the explicit local operation")
            try check(targets().isEmpty && !media.isPublishing && !hasCapture(), "Activation admission cannot start a target, publish media or acquire capture")
            if ["cancel", "later track", "later spoken"].contains(scenario) {
                await process.acknowledgeActivation(); try await Task.sleep(for: .milliseconds(20))
                model.togglePlayback()
            } else if scenario == "runtime" {
                runtime.retry(); try await wait { runtime.status.phase == .ready }
            } else if scenario == "account" {
                model.acceptSpotifyAccess("original-replacement-access")
            } else if scenario == "known account change" {
                try deliverProfile("different-original-listener", refresh: true)
                await process.acknowledgeActivation()
            } else if ["timeout", "active without ACK"].contains(scenario) {
                if scenario == "active without ACK" {
                    await process.clock(selected.id, sequence: 2, rate: 1, position: 60_000, queueEpoch: 7, context: context, complete: true)
                    try await wait { runtime.clock?.sequence == 2 }
                    project(driver, frame(selected, playing: true, context: context, position: 60_000, ownedOutputObserved: true), acknowledge: false)
                    try check(targets().isEmpty && !media.isPublishing && !hasCapture(), "An active endpoint without its activation receipt cannot submit or claim a collection target")
                }
                try await wait { model.spotifyStartingItemID == nil && model.errorMessage != nil }
                try check(started.duration(to: .now) < .milliseconds(650), "The original operation deadline cancels a held activation instead of adding its timeout")
                for _ in 0..<30 {
                    if await process.cancelledActivations.count == 1 { break }
                    try await Task.sleep(for: .milliseconds(10))
                }
                try check(await process.cancelledActivations.count == 1, "Deadline cancellation reaches the exact activation wire")
            } else if scenario == "reject" {
                await process.acknowledgeActivation(accepted: false)
                try await wait { model.spotifyStartingItemID == nil && model.errorMessage != nil }
            } else {
                if scenario == "profile before ACK" { try deliverProfile("original-listener") }
                await process.acknowledgeActivation()
                try await Task.sleep(for: .milliseconds(20))
                if scenario == "profile after ACK" { try deliverProfile("original-listener") }
                try check((scenario == "known inactive" || targets().isEmpty) && !media.isPublishing && !hasCapture(), "An activation ACK alone supplies no device ID or playback ownership")
            }
            if scenario == "no device" {
                try await wait { model.spotifyStartingItemID == nil && model.errorMessage != nil }
            }
            let cannotSubmit = ["cancel", "timeout", "active without ACK", "no device", "runtime", "account", "known account change", "reject", "later track", "later spoken"].contains(scenario)
            await process.acknowledgeActivation()
            var old = frame(selected, playing: true, context: context, position: 60_000, ownedOutputObserved: true)
            old.ownedOutputControl = true; old.observationId = 10
            await process.clock(selected.id, sequence: scenario == "runtime" ? 1 : 2, rate: 1, position: 60_000, queueEpoch: 7, context: context, complete: true)
            try await wait { runtime.clock?.state == .playing }
            project(driver, old, acknowledge: false)
            try check(!media.isPublishing && !hasCapture(), "Old Playing after activation remains unclaimed, including a collection with no item ID")
            if cannotSubmit {
                try await Task.sleep(for: .milliseconds(130))
                try check(targets().isEmpty, "\(scenario) cannot issue a delayed dependent Start when the device later appears")
            } else {
                try await wait { !targets().isEmpty }
            }
            if scenario == "known inactive" {
                let commands = driver.recorded().dropFirst(commandBaseline)
                let start = commands.firstIndex { if case .remote(_, .control(.startOnDevice)) = $0 { return true }; return false }
                try check(start.map { index in
                    !commands[..<index].contains { if case .remote(_, .refresh) = $0 { return true }; return false }
                } == true, "A known inactive device submits Start before any discovery refresh can block the write")
            }
            if scenario == "profile after submit" { try deliverProfile("original-listener") }
            if scenario == "credential after submit" {
                model.acceptSpotifyAccess("original-new-credential-same-account")
                try deliverProfile("original-listener")
            }
            if ["later track", "later spoken"].contains(scenario) {
                await process.clock(selected.id, sequence: 3, state: "paused", rate: 0, position: 60_000, queueEpoch: 7, context: context, complete: true)
                try await wait { runtime.clock?.sequence == 3 }
                project(driver, frame(selected, playing: false, context: context, position: 60_000, ownedOutputObserved: true), acknowledge: false)
                model.togglePlayback()
                try check(targets().contains { control in
                    if scenario == "later spoken", case .startSpokenOnDevice(_, "owned-output", 60_000, _) = control { return true }
                    if scenario == "later track", case .resumeOnDevice(_, "owned-output", 60_000, _, _) = control { return true }
                    return false
                }, "Later explicit Play reuses the real targeted track or spoken route")
            }
            if !cannotSubmit || ["later track", "later spoken"].contains(scenario) {
                let actual = ["track", "known inactive"].contains(scenario) ? target : selected
                let actualContext = scenario == "collection" ? playlist.uri : context
                let actualPosition: UInt64 = ["track", "known inactive", "collection"].contains(scenario) ? 0 : 60_000
                var playing = frame(actual, playing: true, context: actualContext, position: actualPosition, ownedOutputObserved: true)
                playing.ownedOutputControl = true; playing.observationId = 12
                await process.clock(actual.id, sequence: 5, rate: 1, position: actualPosition, queueEpoch: 8, context: actualContext, complete: true)
                try await wait { runtime.clock?.sequence == 5 }
                project(driver, playing)
                try await wait { model.spotifyStartingItemID == nil && model.spotifyStartingContextURI == nil }
                if scenario == "credential after submit" {
                    try check(!media.isPublishing && !hasCapture(), "A new credential epoch cannot confirm an old submitted activation target even for the same account key")
                    await model.prepareToQuit(); continue
                }
                try check(media.isPublishing, "Only the actually submitted and confirmed target can claim the owned media session")
                // Selection invalidation keeps its ordinary 350 ms capture
                // cooldown. A fresh real heartbeat, not the activation ACK,
                // can acquire the lease after that existing fence.
                try await Task.sleep(for: .milliseconds(400))
                await process.clock(actual.id, sequence: 6, rate: 1, position: actualPosition + 400, queueEpoch: 8, context: actualContext, complete: true)
                try await wait { runtime.clock?.sequence == 6 }
                try check(hasCapture(), "A fresh confirmed target may acquire capture after the normal selection cooldown")
            }
            await model.prepareToQuit()
        }
    }
    func ownedInterruptionChecks(_ item: MediaItem) async throws {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let interruption = PlayerCommand.ownedOutputInterrupted(provider: "spotify", generation: 4, sequence: 8, deviceId: "owned-output")
        let wire = try JSONSerialization.jsonObject(with: encoder.encode(interruption)) as! [String: Any]
        try check(wire.count == 5 && wire["type"] as? String == "owned_output_interrupted" && wire["generation"] as? Int == 4 && wire["sequence"] as? Int == 8 && wire["device_id"] as? String == "owned-output" && wire["provider"] as? String == "spotify",
                  "Child interruption carries only the accepted clock scope, without metadata or a public PID")
        let cancelWire = try JSONSerialization.jsonObject(with: encoder.encode(PlayerCommand.cancelOwnedOutputRecovery(provider: "spotify"))) as! [String: Any]
        try check(cancelWire.count == 2 && cancelWire["type"] as? String == "cancel_owned_output_recovery" && cancelWire["provider"] as? String == "spotify", "Explicit reset cancellation is a separate coordinator command")
        for scenario in ["actual", "output first", "supervisor first", "output only", "supervisor only", "retry", "configure", "shutdown", "old generation"] {
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            var events: [SoloistRuntime.ChildInterruption] = []; var resets = 0
            runtime.interrupted = { events.append($0) }; runtime.resetting = { resets += 1 }
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            let generation = runtime.clockGeneration
            switch scenario {
            case "output first", "output only":
                await process.event("{\"event\":\"output_closed\"}")
                try await wait { runtime.status.processID == nil }
                try check(events.isEmpty, "Pipe EOF does not prove that the actual child stopped")
                if scenario == "output first" { await process.exitRuntime() }
            case "supervisor first", "supervisor only":
                await process.event("{\"event\":\"supervisor_exited\",\"code\":10}")
                try await wait { runtime.status.phase == .expired }
                try check(events.isEmpty, "Supervisor termination does not prove that the actual child stopped")
                if scenario == "supervisor first" { await process.exitRuntime() }
            case "retry", "old generation":
                runtime.retry(); try await wait { runtime.clockGeneration > generation && runtime.status.phase == .ready }
                await process.previousEvent("{\"event\":\"exited\",\"code\":10}")
                try await Task.sleep(for: .milliseconds(25))
                try check(events.isEmpty && resets == 1, "Explicit Retry cancels recovery and ignores a previous launch's late exit")
                if scenario == "old generation" { await process.exitRuntime() }
            case "configure":
                runtime.configure(key: "new-original-fixture-key")
                try await wait { runtime.clockGeneration > generation && runtime.status.phase == .ready }
                await process.previousEvent("{\"event\":\"exited\",\"code\":10}")
            case "shutdown":
                await runtime.shutdown(); await process.exitRuntime()
            default: await process.exitRuntime()
            }
            try await Task.sleep(for: .milliseconds(35))
            let actual = ["actual", "output first", "supervisor first", "old generation"].contains(scenario)
            if actual {
                try check(events.count == 1 && events[0].processID == 12345 && events[0].generation == (scenario == "old generation" ? runtime.clockGeneration : generation),
                          "\(scenario): actual exit keeps its immutable launched PID and runtime generation")
                await process.exitRuntime(); try await Task.sleep(for: .milliseconds(20))
                try check(events.count == 1, "A repeated child exit cannot duplicate the interruption")
            } else {
                try check(events.isEmpty, "\(scenario) alone cannot report an unexpected child stop")
                if ["configure", "shutdown"].contains(scenario) { try check(resets == 1, "Explicit reset cancels provenance once before teardown") }
            }
            runtime.interrupted = nil; runtime.resetting = nil; await runtime.shutdown()
        }
        for scenario in ["playing", "paused", "output first", "supervisor first", "supervisor only", "nil only", "unacknowledged", "rejected", "stale", "newer unacknowledged", "retry", "restart", "external"] {
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var drivers: [FixtureConnection] = []
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let driver = FixtureConnection(receive, items: [item], details: .manual, holdOutputClockResults: true)
                drivers.append(driver); return driver
            }, playbackRuntime: runtime, lyricsProvider: OriginalLyricsFixture())
            let driver = drivers[0]
            project(driver, frame(item, device: scenario == "external" ? "external-output" : "owned-output", ownedOutputObserved: scenario != "external"), acknowledge: false)
            await process.clock(item.id, sequence: 1, rate: 1, position: 44_641, queueEpoch: 7, complete: true,
                uptime: scenario == "stale" ? ProcessInfo.processInfo.systemUptime - 2.1 : nil)
            try await wait { runtime.clock?.sequence == 1 }
            func clockRequest(_ sequence: UInt64) -> UInt64? {
                driver.lastRequestID { if case .remote(_, .outputClock(let clock)) = $0 { return clock?.sequence == sequence }; return false }
            }
            if let request = clockRequest(1), scenario != "unacknowledged" {
                driver.receive(.commandResult(CommandOutcome(requestId: request,
                    error: scenario == "rejected" ? PlayerIssue(code: "busy", message: "Original clock admission rejection") : nil)))
            }
            if scenario == "paused" {
                project(driver, frame(item, playing: false, ownedOutputObserved: true), acknowledge: false)
                await process.clock(item.id, sequence: 2, state: "paused", rate: 0, position: 45_500, queueEpoch: 7, complete: true)
                try await wait { runtime.clock?.sequence == 2 }
                driver.receive(.commandResult(CommandOutcome(requestId: clockRequest(2)!, error: nil)))
            }
            if scenario == "newer unacknowledged" {
                await process.clock(item.id, sequence: 2, rate: 1, position: 45_000, queueEpoch: 7, complete: true)
                try await wait { runtime.clock?.sequence == 2 }
            }
            let generation = runtime.clockGeneration
            switch scenario {
            case "output first":
                await process.event("{\"event\":\"output_closed\"}"); try await wait { runtime.clock == nil }
                await process.exitRuntime()
            case "supervisor first", "supervisor only":
                await process.event("{\"event\":\"supervisor_exited\",\"code\":10}"); try await wait { runtime.clock == nil }
                if scenario == "supervisor first" { await process.exitRuntime() }
            case "nil only":
                await process.event("{\"event\":\"clock\",\"clock\":null}")
            case "retry":
                runtime.retry(); try await wait { runtime.status.phase == .ready && runtime.clockGeneration > generation }
                await process.previousEvent("{\"event\":\"exited\",\"code\":10}")
            case "restart": await model.restartPlayer(); await process.exitRuntime()
            default: await process.exitRuntime()
            }
            try await Task.sleep(for: .milliseconds(45))
            let commands = drivers.flatMap { $0.recorded() }
            let scopes = commands.compactMap { command -> (UInt64, UInt64, String)? in
                if case .ownedOutputInterrupted("spotify", let generation, let sequence, let device) = command { return (generation, sequence, device) }; return nil
            }
            let actual = ["playing", "paused", "output first", "supervisor first", "newer unacknowledged"].contains(scenario)
            if actual {
                try check(scopes.count == 1 && scopes[0].0 == generation && scopes[0].1 == (scenario == "paused" ? 2 : 1) && scopes[0].2 == "owned-output",
                          "\(scenario) forwards one exact accepted scope after confirmed actual child termination")
                try check(model.snapshot?.state == (scenario == "paused" ? .paused : .playing), "Swift does not fabricate a paused snapshot before the coordinator restores it")
            } else {
                try check(scopes.isEmpty, "\(scenario) cannot turn an unproven stop into a canonical paused selection")
            }
            if ["retry", "restart"].contains(scenario) {
                try check(commands.contains { if case .cancelOwnedOutputRecovery("spotify") = $0 { return true }; return false }, "Explicit engine replacement cancels recovery even after a nil clock")
            }
            if scenario == "unacknowledged", let request = clockRequest(1) {
                driver.receive(.commandResult(CommandOutcome(requestId: request, error: nil)))
                await process.exitRuntime(); try await Task.sleep(for: .milliseconds(20))
                try check(!driver.recorded().contains { if case .ownedOutputInterrupted = $0 { return true }; return false }, "A late clock ACK cannot rearm an already handled child exit")
            }
            await model.prepareToQuit()
        }
    }
    func ownedRuntimeRestartChecks(_ item: MediaItem) async throws {
        for external in [false, true] {
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var drivers: [FixtureConnection] = []
            let model = PlayerStore(loadLibrary: false, loadSession: false, loadSpotify: true,
                connectionFactory: { receive, _ in
                    let driver = FixtureConnection(receive, items: [item], details: .manual); drivers.append(driver); return driver
                }, playbackRuntime: runtime, lyricsProvider: OriginalLyricsFixture(), spotifyClientID: String(repeating: "a", count: 32),
                spotifyAuthorizationFactory: { status, access, disconnected in
                    SpotifyAuthorization(credentials: FixtureCredentials(), status: status, access: access,
                        disconnected: disconnected, openBrowser: { _ in false })
                })
            let original = drivers[0]
            project(original, frame(item, device: external ? "external-output" : "owned-output", ownedOutputObserved: !external), acknowledge: false)
            await process.clock(item.id, sequence: 1, rate: 1, position: 44_641, queueEpoch: 7, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            let launchesBefore = await process.initialVolumes.count
            await process.holdNextStop()
            let restart = Task { @MainActor in await model.restartPlayer() }
            try await wait { model.isRestarting }
            for _ in 0..<100 {
                if await process.isStopHeld() { break }
                try await Task.sleep(for: .milliseconds(10))
            }
            try check(await process.isStopHeld(), "Restart waits for its exact playback child to stop")
            try check(original.closedCount() == 0, "The owned engine stops before its controlling bridge is discarded")
            try check(!model.canTogglePlayback && !model.canPlay && !model.canGoNext && !model.canGoPrevious && !model.canSeek && !model.canShuffle && !model.canRepeat,
                      "All transport controls are unavailable while restart owns teardown")
            let before = original.recorded().count
            var rejected: [UInt64] = []
            let blockedCommands: [PlayerCommand] = [.play, .pause, .next, .previous, .seek(positionMs: 2_000), .setVolume(0.3), .setShuffle(true), .setRepeat(.one), .remote(provider: "spotify", action: .control(.start(item)))]
            for command in blockedCommands {
                rejected.append(model.send(command))
            }
            model.startSpotifyTrack(item)
            model.next(); model.previous(); model.setVolume(0.4)
            try check(original.recorded().count == before, "Direct transport and catalog Play cannot reach the retiring bridge")
            try await wait { model.lastCommandOutcome?.requestId == rejected.last }
            try check(Set(rejected).count == rejected.count && rejected.allSatisfy { $0 > 0 } && model.lastCommandOutcome?.error?.code == "busy",
                      "Restart transport rejection retains nonzero correlated request identities")
            await process.releaseStop()
            await restart.value
            try await wait { runtime.status.phase == .ready }
            try check(drivers.count == 2 && original.closedCount() == 1, "Restart replaces the player bridge once")
            try check(await process.initialVolumes.count == launchesBefore + 1, "Existing prepareSpotify restores exactly one new runtime without an extra retry")
            try check(!drivers.flatMap { $0.recorded() }.contains {
                if case .remote(_, .control(.pauseOnDevice)) = $0 { return true }
                return false
            }, "Stopping the owned child does not pause an external Connect device")
            try check(runtime.clock == nil && model.ownedSpotifyDevice == nil,
                      "The restarted runtime cannot reuse a previous child's clock or device continuation")
            await model.prepareToQuit()
        }
    }
    func naturalSpatialRetirementChecks(_ item: MediaItem) async throws {
        guard #available(macOS 14.2, *) else { return }
        let context = "spotify:album:" + String(repeating: "c", count: 22)
        let successor = track("s", title: "Original natural successor")
        let scenarios = ["natural marker", "outgoing loading first", "ordinary playing change", "auth", "device",
                         "source", "purpose", "inactive", "seek intent", "unavailable marker",
                         "unavailable outgoing loading first", "unavailable observation only", "unavailable auth",
                         "unavailable seek intent", "unavailable pending shuffle", "unavailable old epoch",
                         "unavailable stale origin", "unavailable without native control"]
        for scenario in scenarios {
            let outgoingLoadingFirst = ["outgoing loading first", "unavailable outgoing loading first"].contains(scenario)
            let suite = "app.auvra.natural-retirement." + UUID().uuidString
            let preferences = UserDefaults(suiteName: suite)!
            defer { preferences.removePersistentDomain(forName: suite) }
            let process = FixtureProcess(clockSupported: true, controlSupported: scenario != "unavailable without native control")
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let driver = FixtureConnection(receive, items: [item, successor], details: .manual)
                fixture = driver; return driver
            }, playbackRuntime: runtime, lyricsProvider: OriginalLyricsFixture(), volumeDefaults: preferences)
            let driver = fixture!
            model.setSpotifySpatialStereo(true)
            var owned = frame(item, context: context, position: item.durationMs - 100,
                              ownedOutputObserved: true)
            owned.ownedOutputControl = true
            project(driver, owned, acknowledge: false, spatialStatus: "active")
            await process.clock(item.id, sequence: 1, rate: 1, position: item.durationMs - 100,
                                queueEpoch: 7, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            guard let oldLease = driver.recorded().compactMap({ command -> HostAnalysisLease? in
                if case .hostAnalysis(let lease) = command { return lease }; return nil
            }).last else { throw Failure(message: "Missing natural retirement origin") }
            let baseline = driver.recorded().count
            if scenario.hasPrefix("unavailable") {
                var outage = frame(item, context: context, position: item.durationMs - 100,
                                   connection: .unavailable, ownedOutputObserved: true)
                outage.ownedOutputControl = scenario != "unavailable observation only"
                project(driver, outage, acknowledge: false, spatialStatus: "active")
            }
            switch scenario {
            case "auth", "unavailable auth":
                let connection: ProviderConnection = scenario == "auth" ? .disconnected : .unavailable
                driver.receive(.remoteState(RemoteState(ownedOutputObserved: true, ownedOutputControl: true,
                    provider: "spotify", connection: connection, authenticationRequired: true,
                    issue: nil, retryAfterSeconds: nil, devices: owned.devices, playback: owned.playback)))
            case "device":
                project(driver, frame(item, device: "external", context: context, ownedOutputObserved: true), acknowledge: false)
            case "source":
                revision += 1
                driver.receive(.snapshot(PlaybackSnapshot(revision: revision, state: .playing, positionMs: 1_000,
                    volume: 0.2, activeProvider: "local", providerConnection: .connected, capabilities: capabilities,
                    queue: QueueState(revision: revision, context: .userQueue, current: item, upcoming: [], history: [],
                                      repeatMode: .off, shuffle: false, playNextCount: 0, forwardCount: 0),
                    transport: .local, playbackSessionActive: true,
                    session: SessionState(enabled: false, ready: true, restored: false, saving: false, issue: nil), queueIssues: [])))
            case "purpose": model.setSpotifySpatialStereo(false)
            case "seek intent", "unavailable seek intent": model.send(.seek(positionMs: item.durationMs - 200))
            case "unavailable pending shuffle": model.send(.setShuffle(true))
            case "unavailable stale origin": try await Task.sleep(for: .milliseconds(850))
            case "outgoing loading first", "unavailable outgoing loading first":
                await process.clock(item.id, sequence: 2, state: "loading", rate: 0,
                    position: item.durationMs - 1, queueEpoch: 7, context: context, complete: true)
                try await wait { runtime.clock?.sequence == 2 }
            default: break
            }
            let successorSequence: UInt64 = outgoingLoadingFirst ? 3 : 2
            await process.clock(successor.id, sequence: successorSequence,
                state: scenario == "ordinary playing change" ? "playing" : "loading",
                rate: scenario == "ordinary playing change" ? 1 : 0,
                active: scenario != "inactive", position: 0, queueEpoch: scenario == "unavailable old epoch" ? 7 : 8)
            try await wait { runtime.clock?.sequence == successorSequence }
            let commands = Array(driver.recorded().dropFirst(baseline))
            let retirements = commands.compactMap { command -> UInt64? in
                if case .retireHostAnalysis(let generation) = command { return generation }; return nil
            }
            let clears = commands.contains { if case .hostAnalysis(nil) = $0 { return true }; return false }
            if scenario == "natural marker" || scenario == "unavailable marker" || outgoingLoadingFirst {
                try check(retirements == [oldLease.generation] && !clears,
                          "\(scenario) retires only the outgoing generation without cancelling core's natural preroll")
                let clockIndex = commands.firstIndex { command in
                    if case .remote("spotify", .outputClock(let clock)) = command { return clock?.sequence == successorSequence }; return false
                }
                let retireIndex = commands.firstIndex { if case .retireHostAnalysis = $0 { return true }; return false }
                try check(clockIndex != nil && retireIndex != nil && clockIndex! < retireIndex!,
                          "The fresh successor clock reaches the coordinator before its scoped retirement")
                let data = try JSONEncoder().encode(PlayerCommand.retireHostAnalysis(generation: oldLease.generation))
                let json = try JSONSerialization.jsonObject(with: data) as! [String: Any]
                try check(json.count == 2 && json["type"] as? String == "retire_host_analysis"
                          && json["generation"] as? UInt64 == oldLease.generation,
                          "Scoped retirement encodes only its type and outgoing generation")
                try check(!commands.contains {
                    if case .hostAnalysis(let lease) = $0 { return lease.map { $0.generation > oldLease.generation || $0.mediaId != item.id } ?? false }
                    return false
                },
                          "A successor Loading never grants a new host lease or capture")
                await process.clock(successor.id, sequence: successorSequence + 1, state: "loading", rate: 0, position: 0, queueEpoch: 8)
                try await wait { runtime.clock?.sequence == successorSequence + 1 }
                let repeated = driver.recorded().dropFirst(baseline)
                try check(repeated.filter { if case .retireHostAnalysis = $0 { return true }; return false }.count == 1
                          && !repeated.contains { if case .hostAnalysis(nil) = $0 { return true }; return false },
                          "Further Loading heartbeats do not cancel or reissue the scoped retirement")
            } else {
                try check(retirements.isEmpty && clears,
                          "\(scenario) keeps the full native host-analysis clear")
            }
            await model.prepareToQuit()
        }
    }

    func spatialClockChangeChecks(_ item: MediaItem) async throws {
        guard #available(macOS 14.2, *) else { return }
        enum Scenario: String, CaseIterable { case fresh, staleTimestamp, repeatedTimestamp, differentEpoch, explicitPause, shufflePending }
        let context = "spotify:album:" + String(repeating: "c", count: 22)
        for scenario in Scenario.allCases {
            let suite = "app.auvra.spatial-clock-change." + UUID().uuidString
            let preferences = UserDefaults(suiteName: suite)!
            defer { preferences.removePersistentDomain(forName: suite) }
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive, items: [item], details: .manual)
                fixture = connection; return connection
            }, playbackRuntime: runtime, volumeDefaults: preferences)
            let driver = fixture!
            model.setSpotifySpatialStereo(true)
            var owned = frame(item, context: context, position: 24_000, ownedOutputObserved: true, positionRate: 1)
            owned.ownedOutputControl = true
            project(driver, owned, acknowledge: false, spatialStatus: "active")

            let originTimestamp = Date().timeIntervalSince1970 * 1000 - (scenario == .staleTimestamp ? 2_000 : 100)
            await process.clock(item.id, sequence: 1, rate: 1, position: 24_000, queueEpoch: 7,
                                sourcePositionTimestampMs: originTimestamp, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            let origin = driver.recorded().compactMap { command -> HostAnalysisLease? in
                if case .hostAnalysis(let lease) = command { return lease }
                return nil
            }.last
            guard let origin, origin.playing, origin.spatialStereo else {
                throw Failure(message: "Missing spatial clock-change origin for \(scenario.rawValue)")
            }
            if scenario == .explicitPause {
                model.send(.pause)
            } else if scenario == .shufflePending {
                model.send(.setShuffle(true))
            }
            let baseline = driver.recorded().count
            let timestamp = switch scenario {
            case .staleTimestamp: originTimestamp + 1_000
            case .repeatedTimestamp: originTimestamp
            default: Date().timeIntervalSince1970 * 1000
            }
            await process.clock(item.id, sequence: 2, rate: 1, position: 23_400,
                queueEpoch: scenario == .differentEpoch ? 8 : 7, sourcePositionTimestampMs: timestamp,
                context: context, complete: true)
            try await wait { runtime.clock?.sequence == 2 }

            let commands = Array(driver.recorded().dropFirst(baseline))
            let clockIndex = commands.firstIndex {
                if case .remote("spotify", .outputClock(let clock)) = $0 { return clock?.sequence == 2 }
                return false
            }
            try check(clockIndex != nil, "The \(scenario.rawValue) clock boundary reaches the bridge")
            let leases = commands.compactMap { command -> HostAnalysisLease? in
                if case .hostAnalysis(let lease) = command { return lease }
                return nil
            }
            let handoffs = commands.enumerated().compactMap { index, command in
                if case .hostAnalysisAfterClockChange(let lease, let retired, let oldSequence, let sequence) = command {
                    return (index: index, lease: lease, retired: retired, origin: oldSequence, sequence: sequence)
                }
                return nil
            }
            let cleared = commands.contains { if case .hostAnalysis(nil) = $0 { return true }; return false }
            if scenario == .fresh {
                guard let handoff = handoffs.first, let clockIndex else {
                    throw Failure(message: "The fresh same-occurrence jump did not replace the spatial lease")
                }
                try check(handoffs.count == 1 && clockIndex < handoff.index
                    && handoff.retired == origin.generation && handoff.origin == 1 && handoff.sequence == 2,
                    "One paired replacement follows the new clock and identifies both exact source sequences")
                try check(!cleared && leases.isEmpty && handoff.lease.generation > origin.generation
                    && handoff.lease.playing && handoff.lease.spatialStereo
                    && handoff.lease.processId == origin.processId && handoff.lease.deviceId == origin.deviceId
                    && handoff.lease.provider == origin.provider && handoff.lease.mediaId == origin.mediaId,
                    "The same owned renderer is eligible for a fresh capture generation without an intervening nil lease")

                let beforeEcho = driver.recorded().count
                await process.clock(item.id, sequence: 3, rate: 1, position: 23_420, queueEpoch: 7,
                    sourcePositionTimestampMs: timestamp, context: context, complete: true)
                try await wait { runtime.clock?.sequence == 3 }
                let echo = driver.recorded().dropFirst(beforeEcho)
                try check(!echo.contains { if case .hostAnalysisAfterClockChange = $0 { return true }; return false }
                    && echo.contains { if case .hostAnalysis(.some(let lease)) = $0 {
                        return lease.generation == handoff.lease.generation
                    }; return false },
                    "An ordinary heartbeat renews the replacement lease without replaying its clock proof")
            } else {
                try check(handoffs.isEmpty, "The \(scenario.rawValue) boundary cannot reuse the spatial renderer")
                if scenario == .explicitPause {
                    try check(leases.allSatisfy { !$0.playing }, "A clock jump cannot reopen output ahead of the explicit Pause")
                } else {
                    try check(cleared && leases.count == 1 && leases[0].generation > origin.generation,
                        "The \(scenario.rawValue) boundary uses the ordinary revoke and reacquire path")
                }
            }
            await model.prepareToQuit()
        }
    }

    func ownedAnalysisChecks(_ item: MediaItem) async throws {
        let saved = UserDefaults.standard.object(forKey: "AuvraReactiveLighting")
        defer { if let saved { UserDefaults.standard.set(saved,forKey: "AuvraReactiveLighting") } else { UserDefaults.standard.removeObject(forKey: "AuvraReactiveLighting") } }
        let process = FixtureProcess(clockSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(),owner: process,deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        var fixture: FixtureConnection?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive,items: [item]); fixture = connection; return connection
        },playbackRuntime: runtime)
        let driver = fixture!
        func leases() -> [HostAnalysisLease] { driver.recorded().compactMap { if case .hostAnalysis(let lease) = $0 { return lease }; return nil } }
        func ownedFrame(playing: Bool = true, connection: ProviderConnection = .connected, observed: Bool? = true,
                        device: String = "owned-output", active: Bool = true, restricted: Bool = false) -> RemoteState {
            frame(item, device: device, playing: playing, connection: connection, ownedOutputObserved: observed, active: active, restricted: restricted)
        }
        model.setReactiveLighting(true)
        project(driver,ownedFrame(playing: false))
        model.send(.play)
        try await Task.sleep(for: .milliseconds(380))
        await process.clock(item.id,sequence: 1)
        try await wait { runtime.clock?.sequence == 1 }
        project(driver,ownedFrame())
        try check(leases().isEmpty,"Owned playback does not capture while the listening surface is hidden")
        model.setAnalysisVisible(true)
        try await wait { !leases().isEmpty }
        let first = leases().last!
        try check(first.processId == 12345 && first.mediaId == item.id && first.deviceId == "owned-output","Analysis lease binds only the authenticated supervisor PID, track and owned device")
        let count = leases().count
        project(driver,ownedFrame())
        try check(leases().count == count,"Duplicate native clocks cannot renew capture or allocate another lease")
        func observation(_ generation: UInt64) -> AudioAnalysisObservation {
            let features = AudioFeatures(playbackPositionMs: 100,rms: 0.02,subBass: 0.01,bass: 0.02,lowMids: 0,mids: 0,presence: 0,highs: 0,transient: 0.1,spectralCentroidHz: 200)
            let frame = AudioAnalysisFrame(generation: generation,sampleRate: 48000,windowFrames: 4096,hopFrames: 960,windowStartFrame: 704,windowEndFrame: 4800,droppedFrames: 0,features: features,spectrum: Array(repeating: 0.01,count: 32))
            let modulation = VisualModulation(generation: generation,sampleRate: 48000,windowEndFrame: 4800,controls: .init(timeSeconds: 0.1,motion: 0.2,turbulence: 0.1,light: 0.1))
            return AudioAnalysisObservation(mediaId: item.id,frame: frame,modulation: modulation)
        }
        project(driver,ownedFrame(),analysis: true)
        driver.receive(.audioAnalysis(observation(first.generation)))
        try check(model.audioAnalysis != nil && model.visualModulation != nil,"A measured frame from the current owned epoch reaches the generic modulation projection")
        project(driver, ownedFrame(), analysisStatus: "tap_unavailable")
        try check(model.isPlaying && model.visualModulation == nil && model.lightingStatus.contains("could not start"),
                  "Unavailable owned audio capture leaves music playing and Frost ambient with an explicit recovery status")
        project(driver, ownedFrame(), analysisStatus: "process_unavailable")
        try check(model.isPlaying && model.visualModulation == nil && model.lightingStatus == "Audio from this playback process isn’t available yet.",
                  "A missing audio process reports its own ambient status while playback continues")
        model.send(.seek(positionMs: 18_000))
        try check(driver.recorded().contains { if case .hostAnalysis(nil) = $0 { return true }; return false },"Seek clears owned analysis before sending the transport intent")
        try await Task.sleep(for: .milliseconds(380))
        await process.clock(item.id,sequence: 2,position: 18_000)
        try await wait { leases().last?.generation != first.generation }
        let afterSeek = leases().last!
        project(driver,ownedFrame(),analysis: true)
        driver.receive(.audioAnalysis(observation(first.generation)))
        try check(model.audioAnalysis == nil && model.visualModulation == nil,"An old pinned ABI event cannot cross into a replacement owned analysis epoch")
        driver.receive(.audioAnalysis(observation(afterSeek.generation)))
        try check(model.visualModulation != nil,"The replacement epoch accepts only its own measured frame")
        try check(afterSeek.generation > first.generation,"A fresh clock after seek opens a new analysis epoch")
        model.setReactiveLighting(false)
        let disabled = driver.recorded().count
        await process.clock(item.id,sequence: 3,position: 18_200)
        try await wait { runtime.clock?.sequence == 3 }
        try check(!driver.recorded().dropFirst(disabled).contains { if case .hostAnalysis(.some) = $0 { return true }; return false },"Music-responsive light off suppresses capture heartbeats")
        model.setReactiveLighting(true)
        try await wait { leases().last!.generation > afterSeek.generation }
        let resumed = leases().last!.generation
        await process.clock(item.id,sequence: 4,state: "paused",position: 18_200)
        try await wait { runtime.clock?.state == .paused }
        project(driver,ownedFrame(playing: false))
        try check(driver.recorded().contains { if case .hostAnalysis(nil) = $0 { return true }; return false },"Paused output releases its analysis lease")
        await process.clock(item.id,sequence: 5,position: 18_200)
        try await wait { runtime.clock?.sequence == 5 }
        project(driver,ownedFrame())
        try await wait { leases().last!.generation > resumed }
        model.transferSpotify("external-output")
        let transferred = leases().count
        await process.clock(item.id,sequence: 6,position: 18_400)
        try await wait { runtime.clock?.sequence == 6 }
        project(driver,ownedFrame(device: "external-output"))
        try check(leases().count == transferred,"External Connect cannot acquire or renew an owned analysis lease")
        model.useAuvraPlayback(); project(driver,ownedFrame())
        try await Task.sleep(for: .milliseconds(380))
        await process.clock(item.id,sequence: 7,position: 19_000)
        try await wait { leases().count > transferred }
        model.setAnalysisVisible(false)
        let hidden = leases().count
        await process.clock(item.id,sequence: 8,position: 19_200)
        try await wait { runtime.clock?.sequence == 8 }
        try check(leases().count == hidden,"Leaving Now Playing releases capture and suppresses background analysis")
        model.send(.seek(positionMs: 19_400))
        runtime.retry(); try await wait { runtime.status.phase == .ready && runtime.clock == nil }
        model.setAnalysisVisible(true)
        try await Task.sleep(for: .milliseconds(380))
        await process.clock(item.id,sequence: 1,position: 2000)
        try await wait { runtime.clock?.sequence == 1 }
        project(driver,ownedFrame(),analysis: true)
        try await wait { leases().count > hidden }
        let restarted = leases().last!
        try check(restarted.generation > afterSeek.generation,"Child restart accepts a new runtime clock sequence without reusing the old epoch")

        var presentedModulation: VisualModulation?
        model.attachVisualModulation { presentedModulation = $0 }
        driver.receive(.audioAnalysis(observation(restarted.generation)))
        let observedOutage = ownedFrame(connection: .unavailable)
        let beforeOutage = leases().count
        project(driver, observedOutage, analysis: true)
        try check(model.audioAnalysis?.generation == restarted.generation && presentedModulation != nil && leases().count == beforeOutage,
                  "An unavailable Web API preserves active Frost modulation from the same authenticated owned output epoch")

        let previousClock = runtime.clock!
        let elapsed = max(0, ProcessInfo.processInfo.systemUptime - previousClock.uptime)
        let nextPosition = previousClock.positionMs + UInt64(elapsed * previousClock.rate * 1000)
        await process.clock(item.id, sequence: 2, position: nextPosition)
        try await wait { runtime.clock?.sequence == 2 }
        try check(leases().count == beforeOutage + 1 && leases().last?.generation == restarted.generation,
                  "An advancing owned clock renews the existing Frost epoch during an observed outage")

        let beforeRevocation = driver.recorded().count
        project(driver, observedOutage, analysis: true, analysisRevokedThrough: restarted.generation)
        try check(model.audioAnalysis == nil && model.visualModulation == nil && presentedModulation == nil,
                  "Coordinator revocation clears the displayed audio and Frost modulation immediately")
        driver.receive(.audioAnalysis(observation(restarted.generation)))
        project(driver, observedOutage, analysis: true)
        try check(model.audioAnalysis == nil && !driver.recorded().dropFirst(beforeRevocation).contains { if case .hostAnalysis = $0 { return true }; return false },
                  "A revoked epoch sends no redundant clear and an old clock or frame cannot rearm it")

        try await Task.sleep(for: .milliseconds(380))
        await process.clock(item.id, sequence: 3, position: nextPosition + 570)
        try await wait { (leases().last?.generation ?? 0) > restarted.generation }
        let recovered = leases().last!
        project(driver, observedOutage, analysis: true, analysisRevokedThrough: restarted.generation)
        driver.receive(.audioAnalysis(observation(recovered.generation)))
        try check(model.audioAnalysis?.generation == recovered.generation && presentedModulation != nil,
                  "A fresh eligible owned clock opens a higher epoch and restores Frost while the Web API is unavailable")

        let beforeLostObservation = driver.recorded().count
        project(driver, ownedFrame(connection: .unavailable, observed: nil), analysis: true)
        try check(model.visualModulation == nil && presentedModulation == nil
                  && driver.recorded().dropFirst(beforeLostObservation).contains { if case .hostAnalysis(nil) = $0 { return true }; return false },
                  "An outage without explicit owned observation releases capture and clears the visual projection")

        let afterLostObservation = leases().count
        project(driver, ownedFrame(observed: nil), analysis: true)
        await process.clock(item.id, sequence: 4)
        try await wait { runtime.clock?.sequence == 4 }
        try check(leases().count == afterLostObservation, "Connected playback alone cannot arm process capture without explicit owned observation")
        project(driver, ownedFrame(active: false), analysis: true)
        project(driver, ownedFrame(restricted: true), analysis: true)
        try check(leases().count == afterLostObservation, "An inactive or restricted device cannot acquire an owned process analysis lease")
        await model.prepareToQuit()
    }
    func spotifySpatialStereoChecks(_ item: MediaItem) async throws {
        guard #available(macOS 14.2, *) else { return }
        let suiteName = "app.auvra.spatial-stereo-fixture"
        let preferences = UserDefaults(suiteName: suiteName)!
        preferences.removePersistentDomain(forName: suiteName)
        let lighting = UserDefaults.standard.object(forKey: "AuvraReactiveLighting")
        defer {
            preferences.removePersistentDomain(forName: suiteName)
            if let lighting { UserDefaults.standard.set(lighting, forKey: "AuvraReactiveLighting") }
            else { UserDefaults.standard.removeObject(forKey: "AuvraReactiveLighting") }
        }
        let process = FixtureProcess(clockSupported: true, controlSupported: true)
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
        runtime.restore()
        try await wait { runtime.status.phase == .ready }
        var fixture: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item])
            fixture = connection
            return connection
        }, playbackRuntime: runtime, volumeDefaults: preferences)
        let driver = fixture!
        func leases() -> [HostAnalysisLease?] {
            driver.recorded().compactMap { command in
                if case .hostAnalysis(let lease) = command { return .some(lease) }
                if case .hostAnalysisAfterSeek(let lease, _, _) = command { return .some(lease) }
                return nil
            }
        }
        func seekProofs() -> [(lease: HostAnalysisLease, request: UInt64, sequence: UInt64)] {
            driver.recorded().compactMap { command in
                guard case .hostAnalysisAfterSeek(let lease, let request, let sequence) = command else { return nil }
                return (lease, request, sequence)
            }
        }
        var owned = frame(item, ownedOutputObserved: true)
        owned.ownedOutputControl = true
        func pausedOwned(_ position: UInt64) -> RemoteState {
            var state = frame(item, playing: false, position: position, ownedOutputObserved: true)
            state.ownedOutputControl = true
            return state
        }
        await process.clock(item.id, sequence: 1, state: "paused", rate: 0)
        try await wait { runtime.clock?.sequence == 1 }
        project(driver, pausedOwned(8_100))
        try check(!model.spotifySpatialStereo && leases().isEmpty, "Spotify spatial stereo is off on a fresh profile")
        model.setSpotifySpatialStereo(true)
        try check(preferences.bool(forKey: "AuvraSpotifySpatialStereo"), "Spotify spatial stereo opt-in persists")
        try check(leases().isEmpty, "A paused selection cannot acquire a relay before playback")
        await process.clock(item.id, sequence: 2, rate: 1, position: 8_000)
        try await wait { runtime.clock?.sequence == 2 }
        project(driver, owned)
        let first = leases().last ?? nil
        try check(first?.spatialStereo == true && first?.visuals == false, "Spotify relay starts independently of the listening surface")
        project(driver, owned, spatialStatus: "waiting_for_route")
        try check(model.spotifySpatialStereoStatus.contains("Bluetooth headphones"), "Spotify relay reports an unsupported output route")
        let fallbackStatuses = [
            "direct", "direct_process", "direct_tap", "direct_tap_device", "direct_tap_format",
            "direct_tap_start", "direct_renderer", "direct_renderer_interrupted", "direct_buffer",
            "direct_input_stalled", "direct_output_stalled"
        ]
        var fallbackMessages = Set<String>()
        for status in fallbackStatuses {
            project(driver, owned, spatialStatus: status)
            fallbackMessages.insert(model.spotifySpatialStereoStatus)
        }
        try check(fallbackMessages.count == fallbackStatuses.count
                  && fallbackMessages.allSatisfy { $0.hasSuffix("Spotify is playing directly.") },
                  "Spotify relay explains each fallback stage and reports direct playback")
        project(driver, owned, spatialStatus: "active")
        try check(model.spotifySpatialStereoStatus.contains("Control Center"), "Active relay points to native spatial stereo controls")
        let beforeVolume = leases().count
        model.setVolume(0.35)
        try check(leases().count == beforeVolume && model.spotifySpatialStereoStatus.contains("Control Center"),
                  "Spotify volume changes keep the native relay and its status")
        model.setReactiveLighting(true)
        model.setAnalysisVisible(true)
        let withVisuals = leases().last ?? nil
        try check(withVisuals?.generation == first?.generation && withVisuals?.visuals == true,
                  "Opening Frost enables analysis without replacing the relay")
        model.setAnalysisVisible(false)
        let hidden = leases().last ?? nil
        try check(hidden?.generation == first?.generation && hidden?.visuals == false,
                  "Closing Frost keeps the same relay running")
        let beforePause = driver.recorded().count
        model.send(.pause)
        let pauseCommands = Array(driver.recorded().dropFirst(beforePause))
        try check(pauseCommands.count >= 2
                  && { if case .hostAnalysis(.some(let lease)) = pauseCommands[0] {
                      return lease.generation == first?.generation && !lease.playing
                  }; return false }()
                  && { if case .pause = pauseCommands[1] { return true }; return false }(),
                  "Pause silences the same relay before the transport write")
        await process.clock(item.id, sequence: 3, rate: 1, position: 8_100)
        try await wait { runtime.clock?.sequence == 3 }
        try check((leases().last ?? nil)?.playing == false, "An in-flight playing clock cannot cancel the pending pause")
        await process.clock(item.id, sequence: 4, state: "paused", rate: 0, position: 0)
        try await wait { runtime.clock?.sequence == 4 }
        project(driver, pausedOwned(8_100), spatialStatus: "paused")
        try check((leases().last ?? nil)?.generation == first?.generation
                  && model.spotifySpatialStereoStatus.contains("resumes"),
                  "A confirmed pause at Soloist's zero position keeps the owned route prepared")
        let beforeSeek = leases().count
        _ = model.seek(milliseconds: 6_100)
        try check(leases().count == beforeSeek && model.latestPositionRequest != nil,
                  "A paused seek is sent without revoking or reactivating the prepared renderer")
        await process.clock(item.id, sequence: 5, state: "paused", rate: 0, position: 6_100)
        try await wait { runtime.clock?.sequence == 5 }
        project(driver, pausedOwned(6_100), spatialStatus: "paused")
        try check((leases().last ?? nil)?.generation == first?.generation && (leases().last ?? nil)?.playing == false,
                  "A confirmed position jump during Pause keeps the same renderer muted")
        let beforeRestart = leases().count
        model.previous()
        let sentRestart = if case .seek(positionMs: 0)? = driver.recorded().last { true } else { false }
        try check(sentRestart && leases().count == beforeRestart,
                  "Previous at over four seconds seeks to zero without dropping the paused renderer")
        model.send(.play)
        await process.clock(item.id, sequence: 6, state: "loading", rate: 0, position: 0)
        try await wait { runtime.clock?.sequence == 6 }
        try check((leases().last ?? nil)?.playing == false, "The relay stays silent while the same track buffers")
        await process.clock(item.id, sequence: 7, rate: 0, position: 0)
        try await wait { runtime.clock?.sequence == 7 }
        try check((leases().last ?? nil)?.generation == first?.generation && (leases().last ?? nil)?.playing == false,
                  "A zero-rate playing notification keeps the same relay silent")
        project(driver, owned, spatialStatus: "active")
        await process.clock(item.id, sequence: 8, rate: 1, position: 36)
        try await wait { runtime.clock?.sequence == 8 }
        try check((leases().last ?? nil)?.generation == first?.generation && (leases().last ?? nil)?.playing == true,
                  "The same relay resumes on a fresh positive-rate clock at the actual position")

        project(driver, owned, spatialStatus: "active")
        let beforeNext = driver.recorded().count
        let beforeNextLease = leases().count
        model.next()
        let nextCommands = driver.recorded().dropFirst(beforeNext)
        try check(nextCommands.contains { if case .next = $0 { return true }; return false }
                  && !nextCommands.contains { if case .hostAnalysis(nil) = $0 { return true }; return false },
                  "Owned Next lets the coordinator revoke capture and prepare the emptied renderer")
        await process.clock(item.id, sequence: 9, rate: 1, position: 114)
        try await wait { runtime.clock?.sequence == 9 }
        try check(leases().count == beforeNextLease, "Late playing clocks cannot reopen the outgoing spatial tap during Next")
        let successor = track("q", title: "Original next spatial fixture")
        var nextFrame = frame(successor, ownedOutputObserved: true)
        nextFrame.ownedOutputControl = true
        await process.clock(successor.id, sequence: 10, rate: 1, position: 30)
        try await wait { runtime.clock?.sequence == 10 }
        project(driver, nextFrame, spatialStatus: "active")
        try check((leases().last ?? nil)?.mediaId == successor.id && (leases().last ?? nil)?.generation != first?.generation,
                  "Next acquires a new capture lease only for the verified successor")

        let beforeNextPause = leases().count
        model.next()
        model.togglePlayback()
        let afterPause = track("r", title: "Original paused successor")
        var afterPausePlaying = frame(afterPause, ownedOutputObserved: true)
        afterPausePlaying.ownedOutputControl = true
        await process.clock(afterPause.id, sequence: 11, rate: 1, position: 20)
        try await wait { runtime.clock?.sequence == 11 }
        project(driver, afterPausePlaying)
        try check(leases().count == beforeNextPause, "Next then Pause withholds a new tap during the successor's brief Playing")
        await process.clock(afterPause.id, sequence: 12, state: "paused", rate: 0, position: 20)
        try await wait { runtime.clock?.sequence == 12 }
        var afterPauseFrame = frame(afterPause, playing: false, ownedOutputObserved: true)
        afterPauseFrame.ownedOutputControl = true
        project(driver, afterPauseFrame)
        try check(leases().count == beforeNextPause, "The paused successor stays outside the audio tap")
        model.togglePlayback()
        await process.clock(afterPause.id, sequence: 13, rate: 1, position: 40)
        try await wait { runtime.clock?.sequence == 13 }
        project(driver, afterPausePlaying)
        try check((leases().last ?? nil)?.mediaId == afterPause.id, "Explicit Play acquires the successor after a rapid Pause")

        let seekEpoch: UInt64 = 6
        func projectPlayingSeek(_ position: UInt64) {
            var state = frame(afterPause, position: position, ownedOutputObserved: true)
            state.ownedOutputControl = true
            project(driver, state, acknowledge: false, spatialStatus: "active")
        }
        func emitPlayingSeekClock(_ sequence: UInt64, _ position: UInt64, sourceTimestamp: Double) async throws {
            await process.clock(afterPause.id, sequence: sequence, rate: 1, position: position,
                                queueEpoch: seekEpoch, sourcePositionTimestampMs: sourceTimestamp)
            try await wait { runtime.clock?.sequence == sequence }
            projectPlayingSeek(position)
        }
        func deliverPlayingSeek(id: UInt64, position: UInt64, sequence: UInt64, accepted: Bool) async throws -> UInt64 {
            guard let request = model.seek(milliseconds: position) else { throw Failure(message: "Missing owned seek request") }
            let command = RemoteOutputCommand(id: id, generation: runtime.clockGeneration, sequence: sequence,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900, requestId: request,
                deviceId: "owned-output", mediaId: afterPause.id,
                control: .init(kind: .seek, positionMs: position, percent: nil, enabled: nil, mode: nil, uri: nil))
            driver.receive(.remoteOutputCommand("spotify", command))
            try await wait {
                driver.recorded().contains { command in
                    guard case .remote(_, .outputCommandResult(let result)) = command else { return false }
                    return result.id == id && result.accepted == accepted
                }
            }
            return request
        }

        let preDispatchTimestamp = Date().timeIntervalSince1970 * 1000
        try await emitPlayingSeekClock(14, 60, sourceTimestamp: preDispatchTimestamp)
        let seekGeneration = (leases().last ?? nil)?.generation
        let seekLeaseCount = leases().count
        let seekStarted = ContinuousClock.now
        let proofCount = seekProofs().count
        let acceptedSeek = try await deliverPlayingSeek(id: 173, position: 15_000, sequence: 14, accepted: true)
        try check(leases().count == seekLeaseCount && seekProofs().count == proofCount,
                  "An accepted playing Seek alone cannot reopen spatial capture")
        try await emitPlayingSeekClock(15, 15_000, sourceTimestamp: preDispatchTimestamp - 1_000)
        try check(leases().count == seekLeaseCount && seekProofs().count == proofCount,
                  "A stale source anchor cannot reopen spatial capture despite a newer host sequence and matching position")
        try await emitPlayingSeekClock(16, 15_000, sourceTimestamp: Date().timeIntervalSince1970 * 1000)
        let confirmedSeek = leases().last ?? nil
        try check(seekStarted.duration(to: .now) < .milliseconds(350) && confirmedSeek?.mediaId == afterPause.id
                  && confirmedSeek?.generation != seekGeneration && confirmedSeek?.playing == true,
                  "A fresh jump confirming the accepted owned Seek acquires a new spatial lease before the fallback cooldown")
        let confirmedProof = seekProofs().last
        try check(seekProofs().count == proofCount + 1 && confirmedProof?.request == acceptedSeek
                  && confirmedProof?.sequence == 16 && confirmedProof?.lease == confirmedSeek,
                  "The first spatial lease carries the exact seek request and the confirming native clock sequence")
        let afterConfirmation = driver.recorded()
        let acknowledgement = afterConfirmation.lastIndex { command in
            guard case .remote(_, .outputCommandResult(let result)) = command else { return false }
            return result.id == 173 && result.accepted
        }
        let confirmingClock = afterConfirmation.lastIndex { command in
            guard case .remote(_, .outputClock(let clock)) = command else { return false }
            return clock?.sequence == 16
        }
        let proofCommand = afterConfirmation.lastIndex { command in
            guard case .hostAnalysisAfterSeek(_, let request, _) = command else { return false }
            return request == acceptedSeek
        }
        guard let acknowledgement, let confirmingClock, let proofCommand else {
            throw Failure(message: "Missing spatial Seek confirmation commands")
        }
        try check(acknowledgement < proofCommand && confirmingClock < proofCommand,
                  "The owner receives the seek acknowledgement and confirming clock before the correlated lease")
        try await emitPlayingSeekClock(17, 15_040, sourceTimestamp: Date().timeIntervalSince1970 * 1000)
        try check(seekProofs().count == proofCount + 1 && (leases().last ?? nil)?.generation == confirmedSeek?.generation,
                  "Later clock updates keep the lease without repeating the seek proof")

        let grantsBeforeRejection = leases().compactMap { $0 }.count
        await process.rejectNextControl()
        _ = try await deliverPlayingSeek(id: 174, position: 8_300, sequence: 17, accepted: false)
        try await emitPlayingSeekClock(18, 8_300, sourceTimestamp: Date().timeIntervalSince1970 * 1000)
        try check(leases().last == .some(nil) && leases().compactMap { $0 }.count == grantsBeforeRejection
                  && seekProofs().count == proofCount + 1,
                  "A fresh matching clock cannot shorten the cooldown after the exact owned Seek was rejected")
        try await Task.sleep(for: .milliseconds(350))
        projectPlayingSeek(8_300)
        try check((leases().last ?? nil)?.playing == true && seekProofs().count == proofCount + 1,
                  "A rejected seek retains normal spatial recovery after the cooldown")

        let grantsBeforeReplacement = leases().compactMap { $0 }.count
        _ = try await deliverPlayingSeek(id: 175, position: 21_000, sequence: 18, accepted: true)
        guard model.seek(milliseconds: 11_000) != nil else { throw Failure(message: "Missing replacement seek request") }
        try await emitPlayingSeekClock(19, 21_000, sourceTimestamp: Date().timeIntervalSince1970 * 1000)
        try check(leases().compactMap { $0 }.count == grantsBeforeReplacement && seekProofs().count == proofCount + 1,
                  "A replacement seek cancels the older confirmation even when its exact target arrives")
        try await Task.sleep(for: .milliseconds(350))
        projectPlayingSeek(21_000)
        try check((leases().last ?? nil)?.playing == true && seekProofs().count == proofCount + 1,
                  "The replacement seek can recover through the normal spatial lease after the cooldown")

        let addressedGeneration = (leases().last ?? nil)?.generation
        let beforeAddressedPause = driver.recorded().count
        let addressedPause = model.send(.remote(provider: "spotify", action: .control(.pauseOnDevice("owned-output"))))
        let addressedCommands = Array(driver.recorded().dropFirst(beforeAddressedPause))
        try check(addressedCommands.count >= 2
                  && { if case .hostAnalysis(.some(let lease)) = addressedCommands[0] {
                      return lease.generation == addressedGeneration && !lease.playing
                  }; return false }()
                  && { if case .remote("spotify", .control(.pauseOnDevice("owned-output"))) = addressedCommands[1] { return true }; return false }()
                  && !addressedCommands.contains { if case .hostAnalysis(nil) = $0 { return true }; return false },
                  "A device-addressed Pause silences the exact existing spatial lease before dispatch without releasing capture")
        driver.receive(.commandResult(.init(requestId: addressedPause,
            error: .init(code: "provider_unavailable", message: "Original addressed Pause rejection"))))
        try check((leases().last ?? nil)?.generation == addressedGeneration && (leases().last ?? nil)?.playing == true,
                  "A rejected addressed Pause releases only its mute intent and recovers the same playing lease")

        let beforeExternal = leases().count
        project(driver, frame(afterPause, device: "external-output", ownedOutputObserved: false), spatialStatus: "active")
        try check(leases().count == beforeExternal + 1 && leases().last == .some(nil)
                  && !model.spotifySpatialStereoStatus.contains("Control Center"),
                  "Transferring to external Connect clears the relay and any active status")
        model.setSpotifySpatialStereo(false)
        try check(!preferences.bool(forKey: "AuvraSpotifySpatialStereo"), "Disabling the relay persists the direct output preference")
        await model.prepareToQuit()
    }
    func rapidSpatialRelativeChecks(_ item: MediaItem) async throws {
        guard #available(macOS 14.2, *) else { return }
        enum Outcome: CaseIterable {
            case previous, next, explicitPrevious, noACK, rejected, wrongRequest, wrongTarget, oldEpoch, originalTarget, noQueue, noAccount, pausedIntent, contextMismatch, justBeforeExpiry, expired
            case advancedPrevious, advancedNext, advancedLoading, staleObservedEpoch, legacyAdvanced
            case partialObserved, partialNoObserved, partialWrongEpoch, partialRequiredOnly
        }
        let middle = track("b", title: "Original rapid predecessor")
        let outgoing = track("c", title: "Original rapid outgoing")
        let other = track("d", title: "Original rapid other")
        let context = "spotify:album:" + String(repeating: "q", count: 22)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        var wire: [String: Any] = ["id":900,"generation":1,"sequence":3,"expires_at_uptime_ms":1000,
            "device_id":"owned-output","media_id":outgoing.id,"control":["kind":"previous"]]
        let legacy = try decoder.decode(RemoteOutputCommand.self, from: JSONSerialization.data(withJSONObject: wire))
        try check(legacy.observedQueueEpoch == nil && legacy.requiredQueueEpoch == nil, "Legacy relative dispatch keeps both epoch fields optional")
        wire["observed_queue_epoch"] = 12
        let observed = try decoder.decode(RemoteOutputCommand.self, from: JSONSerialization.data(withJSONObject: wire))
        try check(observed.observedQueueEpoch == 12 && observed.requiredQueueEpoch == nil, "Observed dispatch epoch decodes without adding native permission")
        for outcome in Outcome.allCases {
            // Drive the original chain and the separately restarted cooldown across
            // their boundaries without depending on MainActor scheduling latency.
            let timed = outcome == .justBeforeExpiry || outcome == .expired
            let start = ContinuousClock.now
            var elapsedMs = 0
            let timing = timed ? SpatialIntentTimeSource(
                continuousNow: { start + .milliseconds(elapsedMs) },
                uptimeNow: { 10 + Double(elapsedMs) / 1000 }
            ) : nil
            let suiteName = "app.auvra.rapid-relative." + UUID().uuidString
            let preferences = UserDefaults(suiteName: suiteName)!
            defer { preferences.removePersistentDomain(forName: suiteName) }
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive, items: [item, middle, outgoing, other], details: outcome == .noAccount ? .manual : .automatic)
                fixture = connection; return connection
            }, playbackRuntime: runtime, volumeDefaults: preferences, spatialIntentTime: timing)
            let driver = fixture!
            func owned(_ track: MediaItem, position: UInt64 = 20, spatialStatus: String = "active") {
                var state = frame(track, context: context, position: position, ownedOutputObserved: true, previous: true, positionRate: 1)
                state.ownedOutputControl = true; project(driver, state, acknowledge: false, spatialStatus: spatialStatus)
            }
            func leases() -> [HostAnalysisLease] {
                driver.recorded().compactMap { if case .hostAnalysis(let lease) = $0 { return lease }; return nil }
            }
            func latestLeaseUpdate() -> HostAnalysisLease? {
                for command in driver.recorded().reversed() {
                    if case .hostAnalysis(let lease) = command { return lease }
                }
                return nil
            }
            model.setSpotifySpatialStereo(true)
            await process.clock(item.id, sequence: 1, rate: 1, position: 2_000, queueEpoch: 10, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }; owned(item, position: 2_000)
            if outcome != .noAccount { try await wait { model.spotifyProfileID != nil } }
            let baseline = leases().count
            try check(baseline > 0, "Rapid relative fixture starts with an actual owned spatial lease")
            model.next()
            if timed {
                elapsedMs = 1040
                await process.clock(item.id, sequence: 2, rate: 1, position: 2_100, queueEpoch: 10, context: context, complete: true)
                try await wait { runtime.clock?.sequence == 2 }; owned(item, position: 2_100)
            }
            let rapidStarted = ContinuousClock.now
            func insideLatestCooldown() -> Bool {
                timed ? elapsedMs < 1390 : ContinuousClock.now - rapidStarted < .milliseconds(350)
            }
            model.next()
            let forward = outcome == .next || timed || outcome == .advancedNext
            if !forward { model.previous() }
            let kind: RemoteOutputCommand.Control.Kind = forward ? .next
                : outcome == .explicitPrevious || outcome == .partialRequiredOnly ? .previousFromStart : .previous
            let request = driver.lastRequestID(matching: { command in
                if forward, case .next = command { return true }
                if !forward, case .skipPrevious = command { return true }
                return false
            })!
            let origin = outgoing
            await process.clock(origin.id, sequence: 3, rate: 1, position: 20, queueEpoch: 12, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 3 }; owned(origin)
            try check(leases().count == baseline, "A rapid relative burst never opens its intermediate occurrence")
            let expected = outcome == .originalTarget ? item : middle
            if outcome != .noQueue {
                let queue: [String: Any] = ["epoch":12,"sequence":3,"media_id":origin.id,
                    "uptime":ProcessInfo.processInfo.systemUptime,
                    "previous":[["media_id":expected.id,"source":"context"]],
                    "upcoming":[["media_id":expected.id,"source":"context"]]]
                let data = try JSONSerialization.data(withJSONObject: ["event":"queue","queue":queue])
                await process.event(String(decoding: data, as: UTF8.self))
                try await wait { runtime.queue?.epoch == 12 }
            }
            let partialOrigin = [.partialObserved, .partialNoObserved, .partialWrongEpoch, .partialRequiredOnly].contains(outcome)
            let advanced = [.advancedPrevious, .advancedNext, .advancedLoading, .staleObservedEpoch, .legacyAdvanced].contains(outcome) || partialOrigin
            if advanced {
                await process.clock(origin.id, sequence: 5, state: outcome == .advancedLoading ? "loading" : "playing",
                                    rate: outcome == .advancedLoading ? 0 : 1, position: 30,
                                    queueEpoch: 12, context: context, complete: !partialOrigin)
                try await wait { runtime.clock?.sequence == 5 }; owned(origin, position: 30)
            }
            var command = RemoteOutputCommand(id: 900, generation: runtime.clockGeneration, sequence: outcome == .partialNoObserved ? 5 : 3,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900,
                requestId: outcome == .wrongRequest ? request + 1 : request, requiredQueueEpoch: kind == .previousFromStart ? 12 : nil,
                deviceId: "owned-output", mediaId: origin.id,
                control: .init(kind: kind, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
            if advanced && ![.legacyAdvanced, .partialNoObserved, .partialRequiredOnly].contains(outcome) {
                command.observedQueueEpoch = outcome == .staleObservedEpoch || outcome == .partialWrongEpoch ? 11 : 12
            }
            if kind == .previousFromStart { command.expectedPreviousMediaId = expected.id }
            if outcome == .noACK { await process.omitNextControlResult() }
            if outcome == .rejected { await process.rejectNextControl() }
            driver.receive(.remoteOutputCommand("spotify", command))
            if outcome != .noACK {
                try await wait { driver.recorded().contains {
                    if case .remote(_, .outputCommandResult(let result)) = $0 { return result.id == command.id }; return false
                } }
            } else {
                for _ in 0..<20 {
                    if await process.sentControlCount > 0 { break }
                    try await Task.sleep(for: .milliseconds(2))
                }
            }
            if timed { elapsedMs = outcome == .justBeforeExpiry ? 1249 : 1250 }
            if outcome == .pausedIntent { model.send(.pause) }
            let target = outcome == .wrongTarget ? other : expected
            let partialSequence: UInt64 = advanced ? 6 : 4
            let finalSequence = partialSequence + 1
            await process.clock(target.id, sequence: partialSequence, rate: 1, position: 30,
                                queueEpoch: outcome == .oldEpoch ? 12 : 13,
                                context: outcome == .partialObserved ? context : nil,
                                complete: outcome != .partialObserved)
            try await wait { runtime.clock?.sequence == partialSequence }; owned(target, position: 30)
            try check(leases().count == baseline, "A partial target clock cannot confirm the rapid relative capture")
            await process.clock(target.id, sequence: finalSequence, rate: 1, position: 30,
                                queueEpoch: outcome == .oldEpoch ? 12 : 13,
                                context: outcome == .contextMismatch ? "spotify:album:" + String(repeating: "r", count: 22) : context,
                                complete: true)
            try await wait { runtime.clock?.sequence == finalSequence }; owned(target, position: 30)
            let positive = [.previous, .next, .explicitPrevious, .justBeforeExpiry, .advancedPrevious, .advancedNext, .advancedLoading, .partialObserved].contains(outcome)
            try check((leases().count > baseline) == positive,
                      "Rapid relative early lease requires latest ACK, exact target and new occurrence: \(outcome)")
            if outcome == .expired {
                elapsedMs = 1389
                owned(target, position: 30)
                try check(leases().count == baseline, "Expired rapid proof remains fenced until the ordinary capture cooldown ends")
                elapsedMs = 1391
                owned(target, position: 30)
                try check(leases().count > baseline && leases().last?.mediaId == middle.id,
                          "The ordinary capture path recovers the same target after the 350 ms cooldown")
            }
            if positive {
                try check(leases().last?.mediaId == middle.id, "Rapid relative early lease belongs only to the final native target")
                try check(insideLatestCooldown(), "Verified rapid relative playback bypasses the original blanket delay")
                let stable = latestLeaseUpdate()
                try check(stable?.generation == leases().last?.generation && stable?.mediaId == middle.id,
                          "An early confirmed relative lease remains live after its proof is consumed")
                let beforeEcho = driver.recorded().count
                owned(target, position: 30, spatialStatus: "starting")
                let echoUpdates = driver.recorded().dropFirst(beforeEcho)
                try check(insideLatestCooldown()
                          && latestLeaseUpdate()?.generation == stable?.generation
                          && !echoUpdates.contains { if case .hostAnalysis(nil) = $0 { return true }; return false }
                          && !echoUpdates.contains { if case .hostAnalysis(.some(let lease)) = $0 { return lease.generation != stable?.generation }; return false },
                          "An immediate same-target status echo keeps the same early lease without revocation or a new generation")
            }
            await model.prepareToQuit()
        }
    }
    func loadingSpatialContinuationChecks(_ item: MediaItem) async throws {
        guard #available(macOS 14.2, *) else { return }
        enum Outcome: CaseIterable { case confirmed, wrongTarget, oldEpoch, missingDispatch }
        let middle = track("b", title: "Original loading predecessor")
        let outgoing = track("c", title: "Original loading target")
        let other = track("d", title: "Original unrelated loading marker")
        let context = "spotify:album:" + String(repeating: "q", count: 22)
        for outcome in Outcome.allCases {
            let suite = "app.auvra.loading-continuation." + UUID().uuidString
            let preferences = UserDefaults(suiteName: suite)!
            defer { preferences.removePersistentDomain(forName: suite) }
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore(); try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive, items: [item, middle, outgoing, other]); fixture = connection; return connection
            }, playbackRuntime: runtime, volumeDefaults: preferences)
            let driver = fixture!
            func owned(_ track: MediaItem) {
                var state = frame(track, context: context, position: 20, ownedOutputObserved: true, previous: true, positionRate: 1)
                state.ownedOutputControl = true; project(driver, state, acknowledge: false, spatialStatus: "starting")
            }
            func leases() -> [HostAnalysisLease] {
                driver.recorded().compactMap { if case .hostAnalysis(let lease) = $0 { return lease }; return nil }
            }
            func queue(_ track: MediaItem, sequence: UInt64, epoch: UInt64, previous: [MediaItem], next: [MediaItem]) async throws {
                let value: [String: Any] = ["epoch":epoch,"sequence":sequence,"media_id":track.id,
                    "uptime":ProcessInfo.processInfo.systemUptime,
                    "previous":previous.map { ["media_id":$0.id,"source":"context"] },
                    "upcoming":next.map { ["media_id":$0.id,"source":"context"] }]
                await process.event(String(decoding: try JSONSerialization.data(withJSONObject: ["event":"queue","queue":value]), as: UTF8.self))
                try await wait { runtime.queue?.epoch == epoch && runtime.queue?.mediaId == track.id }
            }
            func dispatch(_ id: UInt64, request: UInt64, source: MediaItem, sequence: UInt64, epoch: UInt64,
                          kind: RemoteOutputCommand.Control.Kind) async throws {
                var command = RemoteOutputCommand(id: id, generation: runtime.clockGeneration, sequence: sequence,
                    expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900,
                    requestId: request, deviceId: "owned-output", mediaId: source.id,
                    control: .init(kind: kind, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
                command.observedQueueEpoch = epoch
                driver.receive(.remoteOutputCommand("spotify", command))
                try await wait { driver.recorded().contains {
                    if case .remote(_, .outputCommandResult(let result)) = $0 { return result.id == id && result.accepted }; return false
                } }
            }
            model.setSpotifySpatialStereo(true)
            await process.clock(item.id, sequence: 1, rate: 1, position: 2_000, queueEpoch: 10, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }; owned(item)
            try await wait { model.spotifyProfileID != nil }
            try await queue(item, sequence: 1, epoch: 10, previous: [], next: [middle, outgoing])
            let baseline = leases().count
            model.next()
            let first = driver.lastRequestID(matching: { if case .next = $0 { return true }; return false })!
            model.next()
            let second = driver.lastRequestID(matching: { if case .next = $0 { return true }; return false })!
            try await dispatch(870, request: first, source: item, sequence: 1, epoch: 10, kind: .next)
            await process.clock(middle.id, sequence: 3, rate: 1, position: 20, queueEpoch: 11, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 3 }; owned(middle)
            try await queue(middle, sequence: 3, epoch: 11, previous: [item], next: [outgoing])
            if outcome != .missingDispatch {
                try await dispatch(880, request: second, source: middle, sequence: 3, epoch: 11, kind: .next)
            }
            let markerTrack = outcome == .wrongTarget ? other : outgoing
            let marker: [String: Any] = ["media_id":markerTrack.id,"sequence":4,"state":"loading","position_ms":0,
                "rate":0,"active":true,"uptime":ProcessInfo.processInfo.systemUptime,
                "queue_epoch":outcome == .oldEpoch ? 11 : 12,
                "metadata":["title":"Original loading marker","artists":["Original fixture artist"],"duration_ms":240_000]]
            await process.event(String(decoding: try JSONSerialization.data(withJSONObject: ["event":"clock","clock":marker]), as: UTF8.self))
            try await wait { runtime.clock?.sequence == 4 }; owned(markerTrack)
            try check(runtime.clock?.metadata != nil && runtime.clock?.metadata?.contextUri == nil && runtime.clock?.actions == nil,
                      "Loading continuation uses an identified contextless incomplete marker")
            try check(!model.isChangingTrack, "A reconciled selected occurrence does not require a pending skip projection")
            try check(leases().count == baseline, "Loading and intermediate occurrences never acquire a spatial capture")
            let requestedAt = ContinuousClock.now
            model.previous()
            let request = driver.lastRequestID(matching: { if case .skipPrevious = $0 { return true }; return false })!
            await process.clock(outgoing.id, sequence: 5, rate: 1, position: 30, queueEpoch: 12, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 5 }; owned(outgoing)
            try await queue(outgoing, sequence: 5, epoch: 12, previous: [middle, item], next: [other])
            try await dispatch(900, request: request, source: outgoing, sequence: 5, epoch: 12, kind: .previous)
            await process.clock(middle.id, sequence: 6, rate: 1, position: 20, queueEpoch: 13, context: context)
            try await wait { runtime.clock?.sequence == 6 }; owned(middle)
            try check(leases().count == baseline, "Loading continuation still requires full final Playing after its latest ACK")
            await process.clock(middle.id, sequence: 7, rate: 1, position: 30, queueEpoch: 13, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 7 }; owned(middle)
            try check((leases().count > baseline) == (outcome == .confirmed),
                      "Loading continuation requires the actual dispatched target and later occurrence: \(outcome)")
            if outcome == .confirmed {
                try check(leases().last?.mediaId == middle.id && ContinuousClock.now - requestedAt < .milliseconds(350),
                          "A verified delayed Previous after contextless Loading reaches only its final target before the blanket delay")
            }
            await model.prepareToQuit()
        }
    }
    func pausedSpatialPreviousChecks(_ item: MediaItem) async throws {
        guard #available(macOS 14.2, *) else { return }
        enum Outcome: CaseIterable {
            case confirmed, confirmedAfterRewind, rejected, missingTarget, missingAcknowledgement, wrongRequest, wrongSuccessor
        }
        let predecessor = track("q", title: "Original paused spatial predecessor")
        let unrelated = track("r", title: "Original unrelated spatial track")
        let context = "spotify:album:" + String(repeating: "c", count: 22)
        let queueEpoch: UInt64 = 10
        for outcome in Outcome.allCases {
            let suiteName = "app.auvra.paused-spatial-previous-fixture." + UUID().uuidString
            let preferences = UserDefaults(suiteName: suiteName)!
            defer { preferences.removePersistentDomain(forName: suiteName) }
            let process = FixtureProcess(clockSupported: true, controlSupported: true)
            let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture")
            runtime.restore()
            try await wait { runtime.status.phase == .ready }
            var fixture: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive, items: [item, predecessor, unrelated])
                fixture = connection
                return connection
            }, playbackRuntime: runtime, volumeDefaults: preferences)
            let driver = fixture!
            func projectOwned(_ track: MediaItem, playing: Bool, position: UInt64, previous: Bool = false,
                              revoked: UInt64? = nil) {
                var state = frame(track, playing: playing, context: context, position: position,
                                  ownedOutputObserved: true, previous: previous, positionRate: playing ? 1 : 0)
                state.ownedOutputControl = true
                state.observationId = runtime.clock?.sequence
                let status = revoked == nil ? (playing ? "active" : "paused") : "starting"
                project(driver, state, acknowledge: false, spatialStatus: status, analysisRevokedThrough: revoked)
            }
            func leases() -> [HostAnalysisLease] {
                driver.recorded().compactMap { command in
                    if case .hostAnalysis(let lease) = command { return lease }
                    return nil
                }
            }

            model.setSpotifySpatialStereo(true)
            let rewinds = outcome == .confirmedAfterRewind
            let position: UInt64 = rewinds ? 8_000 : 2_500
            await process.clock(item.id, sequence: 1, rate: 1, position: position,
                                queueEpoch: queueEpoch, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 1 }
            projectOwned(item, playing: true, position: position, previous: true)
            model.send(.pause)
            await process.clock(item.id, sequence: 2, state: "paused", rate: 0, position: position,
                                queueEpoch: queueEpoch, context: context, complete: true)
            try await wait { runtime.clock?.sequence == 2 }
            projectOwned(item, playing: false, position: position, previous: true)
            guard let oldLease = leases().last, oldLease.spatialStereo, !oldLease.playing else {
                throw Failure(message: "Missing retained paused spatial lease")
            }
            let originSequence: UInt64 = rewinds ? 3 : 2
            if rewinds {
                model.previous()
                try check(driver.recorded().contains { if case .seek(positionMs: 0) = $0 { return true }; return false },
                          "The first Previous rewinds the currently paused owned track")
                await process.clock(item.id, sequence: originSequence, state: "paused", rate: 0, position: 0,
                                    queueEpoch: queueEpoch, context: context, complete: true)
                try await wait { runtime.clock?.sequence == originSequence }
                projectOwned(item, playing: false, position: 0, previous: true, revoked: oldLease.generation)
                try check(model.snapshot?.state == .paused && model.snapshot?.hostSpatialStatus == "starting"
                          && model.snapshot?.hostAnalysisRevokedThrough == oldLease.generation,
                          "A confirmed paused rewind keeps the owned session with the previous spatial lease revoked")
            }
            try check(runtime.queue == nil && model.snapshot?.queue.deviceQueue == nil && model.canGoPrevious,
                      "Previous remains available without a queue preview or a local queue observation")

            let before = driver.recorded().count
            let started = ContinuousClock.now
            model.previous()
            let commands = driver.recorded().dropFirst(before)
            guard let request = driver.lastRequestID(matching: { if case .skipPrevious = $0 { return true }; return false }) else {
                throw Failure(message: "Missing paused spatial Previous request")
            }
            let revokedAgain = commands.contains { if case .hostAnalysis(nil) = $0 { return true }; return false }
            try check(revokedAgain != rewinds, "Paused Previous revokes an outgoing spatial capture if present")
            var command = RemoteOutputCommand(id: 180, generation: runtime.clockGeneration, sequence: originSequence,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 900,
                requestId: outcome == .wrongRequest ? request + 1 : request,
                requiredQueueEpoch: queueEpoch, deviceId: "owned-output", mediaId: item.id,
                control: .init(kind: .previousFromStart, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
            if outcome != .missingTarget { command.expectedPreviousMediaId = predecessor.id }
            if outcome == .rejected { await process.rejectNextControl() }
            if outcome == .missingAcknowledgement { await process.omitNextControlResult() }
            let previousWrites = await process.sentControlCount
            driver.receive(.remoteOutputCommand("spotify", command))
            func receivedResult(_ accepted: Bool) -> Bool {
                driver.recorded().contains {
                    guard case .remote(_, .outputCommandResult(let result)) = $0 else { return false }
                    return result.id == command.id && result.accepted == accepted
                }
            }
            if outcome == .missingAcknowledgement {
                let deadline = ContinuousClock.now.advanced(by: .milliseconds(200))
                while await process.sentControlCount == previousWrites {
                    guard ContinuousClock.now < deadline else { throw Failure(message: "Missing native Previous write") }
                    try await Task.sleep(for: .milliseconds(5))
                }
                try check(!receivedResult(true) && !receivedResult(false), "The written native Previous has no helper acknowledgement")
            } else {
                try await wait { receivedResult(outcome != .rejected) }
            }
            try check(!leases().contains { $0.mediaId == predecessor.id },
                      "A native Previous receipt alone cannot acquire the predecessor tap")

            let partialSequence = originSequence + 1
            await process.clock(predecessor.id, sequence: partialSequence, rate: 1, position: 20,
                                queueEpoch: queueEpoch + 1, context: context)
            try await wait { runtime.clock?.sequence == partialSequence }
            projectOwned(predecessor, playing: true, position: 20)
            try check(!leases().contains { $0.mediaId == predecessor.id },
                      "An incomplete predecessor clock cannot waive the spatial cooldown")
            let observed = outcome == .wrongSuccessor ? unrelated : predecessor
            let completeSequence = originSequence + 2
            await process.clock(observed.id, sequence: completeSequence, rate: 1, position: 40,
                                queueEpoch: queueEpoch + 1, context: context, complete: true)
            try await wait { runtime.clock?.sequence == completeSequence }
            projectOwned(observed, playing: true, position: 40)
            try check(started.duration(to: .now) < .milliseconds(350), "Fixture reaches complete predecessor before the fallback cooldown")
            if outcome == .confirmed || rewinds {
                let acquired = leases().last
                try check(acquired?.mediaId == predecessor.id && acquired?.generation != oldLease.generation
                          && acquired?.spatialStereo == true && acquired?.playing == true,
                          "Verified wire target acquires the exact predecessor in a new spatial lease without a queue preview or cooldown")
            } else {
                try check(!leases().contains { $0.generation > oldLease.generation },
                          "Unconfirmed native Previous keeps the spatial cooldown (\(outcome))")
            }
            if outcome == .rejected {
                try await Task.sleep(for: .milliseconds(350))
                projectOwned(predecessor, playing: true, position: 40)
                try check(leases().last?.mediaId == predecessor.id && leases().last?.playing == true,
                          "The rejected Previous recovers through ordinary spatial acquisition after cooldown")
            }
            await model.prepareToQuit()
        }
    }
    func listeningPaletteChecks(_ player: PlayerStore) async throws {
        let previous = UserDefaults.standard.object(forKey: "AuvraAnimateSpaces")
        UserDefaults.standard.set(false,forKey: "AuvraAnimateSpaces")
        defer { if let previous { UserDefaults.standard.set(previous,forKey: "AuvraAnimateSpaces") } else { UserDefaults.standard.removeObject(forKey: "AuvraAnimateSpaces") } }
        let still = StillStore()
        try check(still.displayPalette.allSatisfy { $0[0] == $0[1] && $0[1] == $0[2] },"Missing artwork starts with neutral light instead of a colorful preset")
        let cover: [[Float]] = [[0.12,0.46,0.4],[0.3,0.08,0.07],[0.18,0.25,0.42]]
        still.setPalette(cover)
        try check(still.displayPalette == cover,"The static listening fallback uses the supplied cover palette")
        // Freeze the native canvas fallback so the comparison measures theme
        // composition rather than different GPU animation timestamps.
        for scheme in [ColorScheme.dark,.light] {
            try await render(NowPlayingView(player: player,still: still),name: "listening-palette-" + (scheme == .light ? "light" : "dark"),size: NSSize(width: 1180,height: 780),colorScheme: scheme)
        }
        still.setPalette(nil)
        try check(still.displayPalette.allSatisfy { $0[0] == $0[1] && $0[1] == $0[2] },"Clearing artwork returns to neutral light without releasing the scene preset")
        let dark = NSBitmapImageRep(data: try Data(contentsOf: evidence.appendingPathComponent("listening-palette-dark.png")))!
        let light = NSBitmapImageRep(data: try Data(contentsOf: evidence.appendingPathComponent("listening-palette-light.png")))!
        for (index, point) in [(0.08,0.28),(0.11,0.40),(0.10,0.48)].enumerated() {
            let x = Int(Double(dark.pixelsWide) * point.0), y = Int(Double(dark.pixelsHigh) * point.1)
            let a = dark.colorAt(x: x,y: y)!.usingColorSpace(.sRGB)!, b = light.colorAt(x: x,y: y)!.usingColorSpace(.sRGB)!
            let delta = max(abs(a.redComponent-b.redComponent),abs(a.greenComponent-b.greenComponent),abs(a.blueComponent-b.blueComponent))
            try check(delta < 0.02,"Listening palette sample \(index): light and dark keep the same saturated background")
            let channels = [b.redComponent,b.greenComponent,b.blueComponent]
            try check(channels.max()! - channels.min()! > 0.025,"Listening palette sample \(index): the light theme adds no white desaturation veil")
        }
        // A collapsed native toolbar exposes the top safe area. Its dimming
        // must continue across that boundary, including with Reduce Motion.
        still.setPalette(Array(repeating: [0.4, 0.4, 0.4], count: 3))
        for scheme in [ColorScheme.dark, .light] {
            let name = "listening-safe-area-" + (scheme == .light ? "light" : "dark")
            try await render(NowPlayingView(player: player, still: still)
                .safeAreaInset(edge: .top, spacing: 0) { Color.clear.frame(height: 52) },
                name: name, size: NSSize(width: 1180, height: 780), colorScheme: scheme)
            let bitmap = NSBitmapImageRep(data: try Data(contentsOf: evidence.appendingPathComponent(name + ".png")))!
            let scale = Double(bitmap.pixelsWide) / 1180
            let x = Int(1000 * scale)
            let above = bitmap.colorAt(x: x, y: Int(50 * scale))!.usingColorSpace(.sRGB)!
            let below = bitmap.colorAt(x: x, y: Int(54 * scale))!.usingColorSpace(.sRGB)!
            try check(abs(above.redComponent - below.redComponent) < 0.02,
                      "\(name): window dimming stays continuous across the top safe area")
        }
    }
    func playbackArtworkChecks() async throws {
        let fixture = evidence.appendingPathComponent("original-landscape-artwork.png")
        let art = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 16, pixelsHigh: 8, bitsPerSample: 8,
                                   samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                   bytesPerRow: 0, bitsPerPixel: 0)!
        let pixels = art.bitmapData!
        for offset in stride(from: 0, to: art.bytesPerRow * art.pixelsHigh, by: 4) {
            pixels[offset] = 255
            pixels[offset + 1] = 0
            pixels[offset + 2] = 0
            pixels[offset + 3] = 255
        }
        try art.representation(using: .png, properties: [:])!.write(to: fixture)
        let name = "playback-landscape-artwork"
        try await render(PlaybackArtwork(trackID: "original-landscape", reference: fixture.path, direction: 0, distance: 16)
            .frame(width: 160, height: 160).frame(width: 320, height: 280).background(.black),
            name: name, size: NSSize(width: 320, height: 280))
        let bitmap = NSBitmapImageRep(data: try Data(contentsOf: evidence.appendingPathComponent(name + ".png")))!
        let x = bitmap.pixelsWide / 2, y = bitmap.pixelsHigh / 2
        let center = bitmap.colorAt(x: x, y: y)!.usingColorSpace(.sRGB)!
        let margin = bitmap.colorAt(x: x, y: y - bitmap.pixelsWide * 60 / 320)!.usingColorSpace(.sRGB)!
        try check(center.redComponent > 0.75 && center.redComponent - max(center.greenComponent, center.blueComponent) > 0.5,
                  "A non-square playback cover keeps its original picture")
        try check((0.05...0.20).contains(margin.redComponent) && abs(margin.redComponent - margin.greenComponent) < 0.02
                  && abs(margin.redComponent - margin.blueComponent) < 0.02,
                  "Non-square playback art leaves a neutral matte and never reveals the placeholder")
    }
    func transportHitTargetChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let player = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item]); driver = connection; return connection
        })
        let connection = driver!
        project(connection, frame(item, playing: false))
        let size = NSSize(width: 344, height: 126)
        let host = NSHostingView(rootView: TransportButtons(player: player).frame(width: size.width, height: size.height))
        let window = SnapshotWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        defer { window.close() }
        try await Task.sleep(for: .milliseconds(150))
        host.layoutSubtreeIfNeeded()

        func click(_ x: CGFloat, _ y: CGFloat) async throws {
            for type in [NSEvent.EventType.leftMouseDown, .leftMouseUp] {
                guard let event = NSEvent.mouseEvent(with: type, location: NSPoint(x: x, y: y), modifierFlags: [],
                                                     timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber,
                                                     context: nil, eventNumber: 1, clickCount: 1, pressure: type == .leftMouseDown ? 1 : 0) else {
                    throw Failure(message: "Could not create native transport click")
                }
                // Deliver to this fixture window, outside the app-wide input monitor.
                window.sendEvent(event)
            }
            try await Task.sleep(for: .milliseconds(30))
        }
        let first = connection.recorded().count
        try await click(51, 80)
        try await click(293, 46)
        try await click(95, 63)
        let actions = Array(connection.recorded().dropFirst(first))
        guard actions.count == 2, case .setShuffle(true) = actions[0], case .setRepeat(.all) = actions[1] else {
            throw Failure(message: "Expanded transport targets missed an edge click or overlapped the gap")
        }
        try check(true, "Native clicks outside the transport glyphs reach Shuffle and Repeat while the gap stays inert")
        await player.prepareToQuit()
    }

    func lyricFollowInteractionChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let player = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item]); driver = connection; return connection
        }, lyricsProvider: OriginalLyricsFixture())
        let connection = driver!
        project(connection, frame(item, playing: false, position: 4400))
        player.lyrics.setVisible(true)
        try await wait { player.lyrics.result != nil }

        let size = NSSize(width: 370, height: 330)
        let window = SnapshotWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.titled], backing: .buffered, defer: false)
        let host = NSHostingView(rootView: LyricsView(player: player, lyrics: player.lyrics)
            .environment(\.colorScheme, .dark).environment(\.scenePhase, .active).background(.black))
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFront(nil)
        defer { window.close() }
        try await Task.sleep(for: .milliseconds(500))

        func descendants(_ view: NSView) -> [NSView] { [view] + view.subviews.flatMap(descendants) }
        guard let observer = descendants(host).compactMap({ $0 as? LyricsScrollObservationView }).first,
              let scroll = descendants(host).compactMap({ $0 as? NSScrollView }).first else {
            throw Failure(message: "Missing native lyric scroll region")
        }
        let clip = scroll.contentView
        func center(_ view: NSView) -> NSPoint {
            view.convert(NSPoint(x: view.bounds.midX, y: view.bounds.midY), to: nil)
        }
        func rows() -> [NSView] { descendants(host).filter { String(describing: type(of: $0)) == "LyricsDrawingView" } }
        let midpoint = center(clip)
        guard let first = rows().first(where: { abs(center($0).y - midpoint.y) < 3 }) else {
            throw Failure(message: "The first lyric did not center on appearance")
        }

        func scrollDown(_ points: CGFloat) {
            observer.observe(window: window, point: midpoint, deltaX: 0, deltaY: -points)
            let origin = clip.bounds.origin
            clip.scroll(to: NSPoint(x: origin.x, y: origin.y + points))
            scroll.reflectScrolledClipView(clip)
        }
        scrollDown(80)
        try await Task.sleep(for: .milliseconds(1200))
        scrollDown(12)
        try await Task.sleep(for: .milliseconds(1000))
        try check(!player.lyrics.followsPlayback && abs(center(first).y - midpoint.y) > 45,
                  "Additional wheel input extends the manual browse window and preserves its scroll position")
        try await wait { player.lyrics.followsPlayback && abs(center(first).y - midpoint.y) < 3 }
        try check(true, "Karaoke returns to the current cue two seconds after the last wheel input, even while paused")

        scrollDown(70)
        try await Task.sleep(for: .milliseconds(90))
        guard let selected = rows().first(where: {
            $0 !== first && abs(center($0).y - midpoint.y) < 112 && abs(center($0).y - midpoint.y) > 12
        }) else { throw Failure(message: "Missing visible off-center lyric row") }
        for type in [NSEvent.EventType.leftMouseDown, .leftMouseUp] {
            guard let event = NSEvent.mouseEvent(with: type, location: center(selected), modifierFlags: [],
                                                 timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window.windowNumber,
                                                 context: nil, eventNumber: 1, clickCount: 1, pressure: type == .leftMouseDown ? 1 : 0) else {
                throw Failure(message: "Could not create native lyric click")
            }
            window.sendEvent(event)
        }
        guard case .seek(let requested) = connection.recorded().last, requested > 4400 else {
            throw Failure(message: "The browsed lyric click did not seek forward")
        }
        try await wait { abs(center(selected).y - midpoint.y) < 3 }
        try check(player.lyrics.followsPlayback && player.positionMilliseconds() == 4400,
                  "A browsed lyric centers immediately while the old playback clock is still published")
        project(connection, frame(item, playing: false, position: requested))
        try await Task.sleep(for: .milliseconds(520))
        try check(abs(center(selected).y - midpoint.y) < 3,
                  "The lyric stays centered when the delayed seek snapshot arrives")
        await player.prepareToQuit()
    }

    func mainQueryFailureChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        var failure: PlayerFailureReceiver?
        let model = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, rejected in
            let value = FixtureConnection(receive,items: [item],mainMode: .manual); driver = value; failure = rejected; return value
        })
        let connection = driver!
        model.querySpotify(kind: .search,text: "older query"); let older = connection.lastRequestID()
        model.querySpotify(kind: .search,text: "newer query"); let newer = connection.lastRequestID()
        connection.receive(.commandResult(CommandOutcome(requestId: older,error: PlayerIssue(code: "busy",message: "Original older rejection"))))
        try check(model.isLoadingSpotifyPage && model.spotifyQuery.text == "newer query" && model.errorMessage == nil,"A superseded catalogue admission failure cannot replace the newer search or set a playback error")
        connection.receive(.commandResult(CommandOutcome(requestId: newer,error: PlayerIssue(code: "busy",message: "Original current rejection"))))
        try check(!model.isLoadingSpotifyPage && model.spotifyPage?.issue?.message == "Original current rejection" && model.errorMessage == nil,"Current catalogue rejection ends its spinner with a page error")
        let albumID = String(repeating: "a", count: 22)
        model.querySpotify(kind: .albumTracks, text: albumID, offset: 10)
        let albumQuery = model.spotifyQuery
        func albumPage(issue: PlayerIssue?, items: [MediaItem] = []) -> RemotePage {
            RemotePage(provider: "spotify", queryId: albumQuery.queryId, offset: albumQuery.offset,
                total: UInt64(items.count), items: items, collections: [], issue: issue)
        }
        let network = albumPage(issue: PlayerIssue(code: "network_error", message: "Original network failure"))
        connection.receive(.remotePage(network))
        try check(model.spotifyPage?.hasPaginationData == false && model.spotifyPage?.needsCatalogAccountAccess == false,
                  "An unreachable album page has no fabricated count or account permission action")
        let denied = albumPage(issue: PlayerIssue(code: "permission_lost", message: "Original denied page"))
        try check(!denied.hasPaginationData && denied.needsCatalogAccountAccess,
                  "A denied catalog page still offers account permissions")
        let partial = albumPage(issue: PlayerIssue(code: "track_unavailable", message: "Original omitted row"), items: [item])
        try check(partial.hasPaginationData && !partial.needsCatalogAccountAccess && albumPage(issue: nil).hasPaginationData,
                  "Partial results and a real empty result still have valid pagination data")
        model.querySpotify(kind: .search,text: "bridge admission"); let bridgeRequest = connection.lastRequestID()
        failure?(bridgeRequest,"Original bridge admission rejection")
        try check(!model.isLoadingSpotifyPage && model.spotifyPage?.issue?.message == "Original bridge admission rejection" && model.errorMessage == nil,"Bridge admission rejection finishes only its current catalogue page")
        model.querySpotify(kind: .search,text: String(repeating: "🎶",count: 300))
        try check(model.spotifyQuery.text.utf8.count <= 512 && model.spotifyQuery.text.count <= 256 && !model.spotifyQuery.text.contains("�"),"Search limits preserve valid Unicode within provider byte and character bounds")
        connection.receive(.commandResult(CommandOutcome(requestId: connection.lastRequestID(),error: nil)))
        failure?(nil,"Original core connection loss")
        try check(model.bridgeFailed && !model.isLoadingSpotifyPage && model.spotifyPage?.issue?.message == "Original core connection loss","A core failure also finishes an admitted catalogue read that has no page yet")
        await model.prepareToQuit()
        var boundedDriver: FixtureConnection?
        let bounded = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in
            let value = FixtureConnection(receive,items: [item],mainMode: .manual); boundedDriver = value; return value
        })
        for index in 0..<65 { bounded.querySpotify(kind: .search,text: "Original bounded query \(index)") }
        try check(boundedDriver!.recorded().count == 64 && !bounded.isLoadingSpotifyPage && bounded.spotifyPage?.issue?.code == "busy","Catalogue admission tracking is bounded and overflow finishes with a retryable page error")
        await bounded.prepareToQuit()
    }
    func cursorCatalogNavigationChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item], mainMode: .manual)
            driver = connection
            return connection
        })
        let connection = driver!
        func deliver(_ query: RemoteQuery, next: String? = nil, issue: PlayerIssue? = nil) {
            connection.receive(.remotePage(RemotePage(nextCursor: next, provider: "spotify", queryId: query.queryId,
                offset: query.offset, total: 100, items: issue == nil ? [item] : [], collections: [], issue: issue)))
        }
        func finish(next: String? = nil, issue: PlayerIssue? = nil) {
            connection.receive(.commandResult(CommandOutcome(requestId: connection.lastRequestID(), error: nil)))
            deliver(model.spotifyQuery, next: next, issue: issue)
        }

        let recentCursor = "1793894610000"
        model.presentSpotifyCatalog(kind: .recent, text: "")
        let firstHistory = model.spotifyQuery
        finish(next: recentCursor)
        model.spotifyPageBy(1)
        let secondHistory = model.spotifyQuery
        model.spotifyPageBy(1)
        try check(secondHistory.text == recentCursor && secondHistory.offset == 10 && model.spotifyQuery.queryId == secondHistory.queryId,
                  "History keeps Spotify's exact next cursor and rejects a second Next while its page is pending")
        finish()
        model.presentSpotifyCatalog(kind: .playlistItems, text: String(repeating: "p", count: 22))
        finish()
        model.presentSpotifyCatalog(kind: .recent, text: "")
        let reopened = model.spotifyQuery
        deliver(firstHistory, next: "1793894000000")
        try check(reopened.text == recentCursor && reopened.offset == 10 && reopened.queryId > secondHistory.queryId
                  && model.isLoadingSpotifyPage && !model.spotifyCanAdvanceCursor,
                  "Returning from a playlist reloads the same history page and ignores the older result")
        finish(issue: PlayerIssue(code: "unavailable", message: "Original history retry"))
        model.presentSpotifyCatalog(kind: .recent, text: "")
        try check(model.spotifyQuery.text == recentCursor && model.spotifyQuery.offset == 10,
                  "Retry preserves the current history cursor")
        finish(next: recentCursor)
        try check(!model.spotifyCanAdvanceCursor, "A repeated history cursor cannot trap Next in a cycle")
        model.spotifyPageBy(-1)
        try check(model.spotifyQuery.text.isEmpty && model.spotifyQuery.offset == 0,
                  "Previous Page returns to the first history request with no cursor")
        finish(next: String(repeating: "x", count: 513))
        try check(!model.spotifyCanAdvanceCursor, "An oversized cursor is rejected before its identity could be truncated")

        let artistCursor = String(repeating: "a", count: 22)
        model.presentSpotifyCatalog(kind: .followedArtists, text: "")
        finish(next: artistCursor)
        model.spotifyPageBy(1)
        finish()
        model.presentSpotifyCatalog(kind: .recent, text: "")
        finish()
        model.presentSpotifyCatalog(kind: .followedArtists, text: "")
        try check(model.spotifyQuery.text == artistCursor && model.spotifyQuery.offset == 10,
                  "Followed Artists keeps its own position independently of Recently Played")
        finish()
        model.acceptSpotifyAccess("original-fixture-access")
        try check(model.spotifyQuery.kind == .followedArtists && model.spotifyQuery.offset == 0 && model.spotifyQuery.text.isEmpty,
                  "A new authorization starts the cursor route from the first page")
        await model.prepareToQuit()
    }
    func rapidLocalPlaybackToggles() async throws {
        let capabilities = MediaCapabilities(play: true, pause: true, seek: true, volume: true, pcmAnalysis: true, systemMediaSession: true)
        let item = MediaItem(id: "local:track:toggle", title: "Original local toggle", artists: [], album: nil,
                             durationMs: 60000, artwork: nil, capabilities: capabilities)
        for cold in [true, false] {
            var driver: FixtureConnection?
            let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
                let connection = FixtureConnection(receive, items: [item]); driver = connection; return connection
            })
            let connection = driver!
            let media = MediaPlayerSystemSession(player: model)
            model.attachSystemMedia(media)
            func observe(_ state: PlaybackState, active: Bool = false) {
                revision += 1
                connection.receive(.snapshot(PlaybackSnapshot(revision: revision, state: state, positionMs: 1200,
                    volume: 0.3, activeProvider: "local", providerConnection: .connected, capabilities: capabilities,
                    queue: QueueState(revision: revision, context: .userQueue, current: item, upcoming: [], history: [],
                                      repeatMode: .off, shuffle: false, playNextCount: 0, forwardCount: 0),
                    transport: .local, playbackSessionActive: active,
                    session: SessionState(enabled: cold, ready: true, restored: cold, saving: false, issue: nil), queueIssues: [])))
            }
            observe(.paused, active: !cold)
            let baseline = connection.recorded().count
            model.togglePlayback()
            let play = connection.lastRequestID()
            try check(model.playbackToggleShowsPause && model.canTogglePlayback && !model.isPlaying,
                      "An admitted local Play can be paused before the first transport snapshot")
            if cold {
                connection.receive(.commandResult(.init(requestId: play, error: nil)))
                observe(.loading)
                try check(model.playbackToggleShowsPause && model.canTogglePlayback && !media.isPublishing,
                          "Cold local loading still accepts Pause and does not claim the system media session")
                try check(model.seek(milliseconds: 12000) != nil && model.playbackToggleShowsPause,
                          "A cold seek keeps the same recording's pending local Play intent")
            }
            model.togglePlayback()
            let pause = connection.lastRequestID()
            let verbs = connection.recorded().dropFirst(baseline).compactMap { command -> String? in
                switch command { case .play: "play"; case .pause: "pause"; default: nil }
            }
            try check(verbs == ["play", "pause"] && !model.playbackToggleShowsPause,
                      "Two local toggles before confirmation send Play and then Pause")
            if !cold { connection.receive(.commandResult(.init(requestId: play, error: nil))) }
            observe(cold ? .loading : .playing, active: !cold)
            try check(!model.playbackToggleShowsPause && model.canTogglePlayback,
                      "A snapshot before the latest local receipt cannot undo the pending Pause")
            connection.receive(.commandResult(.init(requestId: pause, error: nil)))
            observe(.paused, active: true)
            observe(.playing, active: true)
            try check(model.playbackToggleShowsPause && model.isPlaying,
                      "A settled local Pause releases the button to later actual transport state")
            model.togglePlayback()
            let rejected = connection.lastRequestID()
            connection.receive(.commandResult(.init(requestId: rejected,
                error: PlayerIssue(code: "unavailable", message: "Original local toggle rejection"))))
            try check(model.playbackToggleShowsPause, "A rejected local Pause releases its pending button state")
            model.togglePlayback()
            model.send(.replaceQueue(items: [item], start: 0, context: .userQueue))
            observe(.loading)
            try check(!model.canTogglePlayback, "Replacing the queue discards the prior Play or Pause even for the same media ID")
            await model.prepareToQuit()
        }
    }

    func localSkipHandoffChecks() async throws {
        let capabilities = MediaCapabilities(play: true, pause: true, seek: true, volume: true, pcmAnalysis: true, systemMediaSession: true)
        func localTrack(_ id: String) -> MediaItem {
            MediaItem(id: "local:track:\(id)", title: "Original local \(id)", artists: [], album: nil,
                      durationMs: 60000, artwork: nil, capabilities: capabilities)
        }
        let first = localTrack("first"), second = localTrack("second")
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [first, second]); driver = connection; return connection
        })
        let connection = driver!
        func observe(_ item: MediaItem, _ state: PlaybackState, upcoming: [MediaItem] = [], provider: String = "local", restored: Bool = false, position: UInt64? = nil) {
            revision += 1
            connection.receive(.snapshot(PlaybackSnapshot(revision: revision, state: state,
                positionMs: position ?? (state == .playing ? 1000 : 0), volume: 0.3, activeProvider: provider,
                providerConnection: .connected, capabilities: item.capabilities,
                queue: QueueState(revision: revision, context: .userQueue, current: item, upcoming: upcoming,
                                  history: [], repeatMode: .off, shuffle: false, playNextCount: 0, forwardCount: 0),
                transport: .local, playbackSessionActive: !restored && state != .loading,
                session: SessionState(enabled: restored, ready: true, restored: restored, saving: false, issue: nil), queueIssues: [])))
        }
        func verbs(after start: Int) -> [String] {
            connection.recorded().dropFirst(start).compactMap { command in
                switch command { case .next: "next"; case .pause: "pause"; case .play: "play"; default: nil }
            }
        }
        for startedPlaying in [false, true] {
            let initial: PlaybackState = startedPlaying ? .playing : .paused
            observe(first, initial, upcoming: [second], position: 59_900)
            let baseline = connection.recorded().count
            model.togglePlayback()
            let beforeSkip = connection.lastRequestID()
            model.next()
            let skip = connection.lastRequestID()
            model.togglePlayback()
            let afterSkip = connection.lastRequestID()
            let expected = startedPlaying ? ["pause", "next", "play"] : ["play", "next", "pause"]
            try check(verbs(after: baseline) == expected && model.playbackToggleShowsPause == startedPlaying,
                      "Next carries the latest unconfirmed local toggle into the following toggle")
            for request in [beforeSkip, skip, afterSkip] {
                connection.receive(.commandResult(.init(requestId: request, error: nil)))
            }
            observe(second, initial)
        }

        observe(first, .playing, upcoming: [second])
        let next = connection.recorded().count
        model.next()
        model.togglePlayback()
        let pause = connection.lastRequestID()
        try check(verbs(after: next) == ["next", "pause"] && model.isChangingTrack && !model.playbackToggleShowsPause,
                  "Next followed immediately by Space pauses the selected local recording")
        observe(second, .loading)
        observe(second, .playing)
        try check(model.canTogglePlayback && !model.playbackToggleShowsPause && !model.canSeek,
                  "Earlier local loading and playing snapshots preserve the pending Pause action")
        connection.receive(.commandResult(CommandOutcome(requestId: pause, error: nil)))
        observe(second, .paused, upcoming: [first])
        try check(!model.isChangingTrack && !model.playbackToggleShowsPause && model.canSeek,
                  "The paused local observation closes the handoff and restores seeking")

        let forward = connection.recorded().count
        model.seek(milliseconds: 9000)
        model.previous()
        let forwardSequence = connection.recorded().dropFirst(forward).compactMap { command -> String? in
            switch command { case .seek(positionMs: 9000): "seek"; case .previous: "previous"; case .skipPrevious: "skip"; default: nil }
        }
        try check(forwardSequence == ["seek", "previous"],
                  "A fast Previous delegates the 4-second rule after the admitted local seek, even while the visible pause is at zero")
        connection.receive(.commandResult(.init(requestId: connection.lastRequestID(), error: nil)))
        observe(second, .paused, position: 0)

        observe(second, .paused, position: 6000)
        let repeated = connection.recorded().count
        guard let repeatSeek = model.seek(milliseconds: 9000) else { throw Failure(message: "Local seek was not admitted") }
        connection.receive(.commandResult(.init(requestId: repeatSeek, error: nil)))
        observe(second, .loading, position: 9000)
        model.previous()
        let firstPrevious = connection.lastRequestID()
        observe(second, .paused, position: 9000)
        model.previous()
        let repeatedPrevious = connection.recorded().dropFirst(repeated).filter { command in
            if case .previous = command { return true }
            return false
        }.count
        try check(repeatedPrevious == 2, "A stale local seek snapshot cannot turn two Previous presses into restarts of the same file")
        connection.receive(.commandResult(.init(requestId: firstPrevious, error: nil)))
        connection.receive(.commandResult(.init(requestId: connection.lastRequestID(), error: nil)))
        observe(first, .paused)

        observe(second, .playing, position: 9000)
        guard let olderSeek = model.seek(milliseconds: 7000), let replacementSeek = model.seek(milliseconds: 1000) else {
            throw Failure(message: "Rapid local seeks were not admitted")
        }
        connection.receive(.commandResult(.init(requestId: olderSeek, error: nil)))
        observe(second, .playing, position: 7000)
        connection.receive(.commandResult(.init(requestId: replacementSeek, error: nil)))
        observe(second, .loading, position: 1000)
        model.previous()
        guard case .previous? = connection.recorded().last else {
            throw Failure(message: "An older playing snapshot or receipt overrode the latest local seek")
        }
        try check(model.current?.id == second.id, "Previous behind rapid local seeks uses the accepted engine position during same-file loading")
        connection.receive(.commandResult(.init(requestId: connection.lastRequestID(), error: nil)))

        observe(second, .paused, position: 2000)
        model.seek(milliseconds: 9000)
        model.previous()
        observe(second, .paused, provider: "other-local", position: 2000)
        model.previous()
        guard case .skipPrevious? = connection.recorded().last else {
            throw Failure(message: "A local provider change carried a delegated Previous to the same media ID")
        }
        observe(first, .paused, provider: "other-local")

        observe(second, .paused, position: 2000)
        guard let failedSeek = model.seek(milliseconds: 9000) else { throw Failure(message: "Local seek for rejection test was not admitted") }
        connection.receive(.commandResult(.init(requestId: failedSeek, error: .init(code: "unavailable", message: "Fixture seek rejection"))))
        model.previous()
        guard case .skipPrevious? = connection.recorded().last else { throw Failure(message: "Rejected local seek still controls Previous") }
        observe(first, .paused)
        observe(second, .paused, position: 2000)
        guard let settledSeek = model.seek(milliseconds: 4000) else { throw Failure(message: "Local seek for settled test was not admitted") }
        connection.receive(.commandResult(.init(requestId: settledSeek, error: nil)))
        observe(second, .paused, position: 4000)
        model.previous()
        guard case .seek(positionMs: 0)? = connection.recorded().last else {
            throw Failure(message: "Confirmed local position at 4000 ms did not restore the ordinary Previous path")
        }
        try check(model.current?.id == second.id, "Failed and confirmed local seeks retire the special Previous route")
        model.send(.replaceQueue(items: [second, first], start: 0, context: .userQueue))
        observe(second, .paused, upcoming: [first])
        model.previous()
        guard case .skipPrevious? = connection.recorded().last else { throw Failure(message: "Same-item queue replacement retained a prior seek") }
        observe(first, .paused)
        observe(second, .paused, upcoming: [first])

        let seeks = connection.recorded().count
        guard let firstSeek = model.seek(milliseconds: 3000) else { throw Failure(message: "First local seek was not admitted") }
        connection.receive(.commandResult(CommandOutcome(requestId: firstSeek, error: nil)))
        observe(second, .loading, upcoming: [first])
        guard let latestSeek = model.seek(milliseconds: 12000) else { throw Failure(message: "Replacement local seek was not admitted") }
        let seekTargets = connection.recorded().dropFirst(seeks).compactMap { command -> UInt64? in
            if case .seek(let position) = command { return position }
            return nil
        }
        try check(seekTargets == [3000, 12000],
                  "A second seek on the same local recording reaches the engine while the first seek loads")
        connection.receive(.commandResult(CommandOutcome(requestId: latestSeek, error: nil)))
        observe(second, .paused, upcoming: [first])
        observe(second, .loading, upcoming: [first])
        try check(!model.canSeek, "A completed seek does not unlock a later load of the same recording")

        observe(second, .paused, upcoming: [first], restored: true)
        model.togglePlayback()
        let start = connection.lastRequestID()
        connection.receive(.commandResult(CommandOutcome(requestId: start, error: nil)))
        observe(second, .loading, upcoming: [first], restored: true)
        guard let initialSeek = model.seek(milliseconds: 42000) else {
            throw Failure(message: "First seek during a restored local Play was not admitted")
        }
        let sentInitialSeek: Bool
        if case .seek(positionMs: 42000)? = connection.recorded().last { sentInitialSeek = true }
        else { sentInitialSeek = false }
        try check(sentInitialSeek, "The first seek reaches the engine while an explicitly resumed local recording opens")
        connection.receive(.commandResult(CommandOutcome(requestId: initialSeek, error: nil)))
        observe(second, .paused, upcoming: [first], restored: true)
        model.togglePlayback()
        connection.receive(.commandResult(CommandOutcome(requestId: connection.lastRequestID(), error: nil)))
        model.send(.replaceQueue(items: [second, first], start: 0, context: .userQueue))
        observe(second, .loading, upcoming: [first])
        try check(!model.canSeek, "A replacement queue cancels the local Play seek permission even for the same recording")

        observe(second, .paused, upcoming: [first])

        let toggles = connection.recorded().count
        model.next()
        observe(first, .loading)
        try check(model.seek(milliseconds: 12000) == nil, "Seeking stays disabled during a local track change")
        model.togglePlayback()
        model.togglePlayback()
        try check(verbs(after: toggles) == ["next", "play", "pause"] && !model.playbackToggleShowsPause,
                  "Two Space presses while the next paused local item loads preserve their order")
        connection.receive(.commandResult(CommandOutcome(requestId: connection.lastRequestID(),
            error: PlayerIssue(code: "unavailable", message: "Original local pause rejection"))))
        try check(!model.isChangingTrack && !model.canTogglePlayback,
                  "A rejected toggle clears its local pending intent")

        let mock = track("q")
        observe(first, .playing, upcoming: [mock])
        let mixed = connection.recorded().count
        model.next()
        model.togglePlayback()
        try check(verbs(after: mixed) == ["next"], "A known non-PCM destination does not inherit the local handoff")
        observe(mock, .playing, provider: "mock")
        let demo = connection.recorded().count
        model.next()
        model.togglePlayback()
        try check(verbs(after: demo) == ["next"], "Silent demo handoff keeps the existing toggle gate")
        await model.prepareToQuit()
    }
    func fastSwitchAndVolumeChecks(_ item: MediaItem) async throws {
        let suite = "app.auvra.switch-volume." + UUID().uuidString
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let process = FixtureProcess()
        let runtime = SoloistRuntime(credentials: FixtureCredentials(), owner: process, deviceName: "Auvra fixture", volumeDefaults: defaults)
        runtime.rememberVolume(0.61)
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        let next = track("k", title: "Original upcoming recording")
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let value = FixtureConnection(receive, items: [item, next]); driver = value; return value
        }, playbackRuntime: runtime, volumeDefaults: defaults)
        let connection = driver!
        let deviceQueue = DeviceQueueDetails(previous: [], previousSources: nil, upcomingSources: [.queue])
        project(connection, frame(item, playing: false, position: 6500, volume: 0.61), deviceQueue: deviceQueue, upcoming: [next])
        model.next()
        try check(model.displayedTrack?.id == next.id && model.current?.id == item.id && model.displayedPosition == 0
                  && model.position() == 6.5 && model.trackTransitionDirection == 1 && model.isLoading && !model.canSeek,
                  "Next presents the owned queue candidate immediately while keeping the real clock and seek fenced")
        connection.receive(.commandResult(CommandOutcome(requestId: connection.lastRequestID(), error: PlayerIssue(code: "unavailable", message: "Original skip rejected"))))
        try check(model.displayedTrack?.id == item.id && !model.isChangingTrack && model.trackTransitionDirection == 0,
                  "A rejected skip rolls presentation back to the actual track")
        let third = track("t", title: "Original second upcoming recording")
        var owned = frame(item, position: 6500, ownedOutputObserved: true, volume: 0.61, previous: true)
        owned.ownedOutputControl = true
        project(connection, owned, deviceQueue: deviceQueue, upcoming: [next])
        let doubleIndex = connection.recorded().count
        model.togglePlayback(); model.togglePlayback()
        let doubleToggle = connection.recorded().dropFirst(doubleIndex).compactMap { command -> String? in
            switch command { case .pause: "pause"; case .play: "play"; default: nil }
        }
        try check(doubleToggle == ["pause", "play"] && model.playbackToggleShowsPause,
                  "Two rapid Space presses send the opposite intents before a new playback snapshot")
        let toggleIndex = connection.recorded().count
        model.next(); model.togglePlayback(); model.togglePlayback()
        let toggles = connection.recorded().dropFirst(toggleIndex).compactMap { command -> String? in
            switch command { case .next: "next"; case .pause: "pause"; case .play: "play"; default: nil }
        }
        try check(toggles == ["next", "pause", "play"] && model.displayedTrack?.id == next.id && model.isLoading && !model.canSeek,
                  "Owned Next preserves the preview and admits rapid Pause then Play without admitting an old-track seek")
        let previousIndex = connection.recorded().count
        model.previous()
        try check(connection.recorded().dropFirst(previousIndex).contains { if case .skipPrevious = $0 { true } else { false } },
                  "Previous immediately after Next does not seek the old recording")
        project(connection, frame(next, position: 0, volume: 0.61))
        let menu = AuvraMenuPresentation(), navigation = AppNavigation(), windows = MacWindowService()
        var firstTrack = frame(item, playing: false, position: 2000, ownedOutputObserved: true)
        firstTrack.ownedOutputControl = true
        project(connection, firstTrack)
        menu.connect(player: model, navigation: navigation, windows: windows)
        let deniedPrevious = connection.recorded().count
        model.previous()
        try check(!model.canGoPrevious && !menu.state.canGoPrevious && connection.recorded().count == deniedPrevious,
                  "Owned first-track Previous stays unavailable before 4 s while playback is stable")
        model.next()
        try await wait { menu.state.canGoPrevious }
        try check(model.canGoPrevious && !model.canSeek && model.displayedTrack?.id == item.id,
                  "An owned Next enables the Previous shortcut before queue details arrive")
        project(connection, frame(next, position: 300, volume: 0.61, previous: true))

        func firstTrackAt(_ position: UInt64, device: String = "owned-output") -> RemoteState {
            var state = frame(item, device: device, position: position, ownedOutputObserved: device == "owned-output")
            state.ownedOutputControl = device == "owned-output"
            return state
        }
        project(connection, firstTrackAt(35_000), deviceQueue: deviceQueue, upcoming: [next])
        let seekThenNavigation = connection.recorded().count
        model.seek(milliseconds: 15_000)
        model.next()
        project(connection, firstTrackAt(15_000), deviceQueue: deviceQueue, upcoming: [next])
        try check(model.isChangingTrack && model.displayedTrack?.id == next.id && model.canGoPrevious && !model.canSeek,
                  "An earlier backward seek cannot settle the pending switch to a distinct owned queue item")
        model.previous()
        model.togglePlayback()
        let seekThenNavigationVerbs = connection.recorded().dropFirst(seekThenNavigation).compactMap { command -> String? in
            switch command { case .seek(positionMs: 15_000): "seek"; case .next: "next"; case .skipPrevious: "previous"; case .pause: "pause"; default: nil }
        }
        try check(seekThenNavigationVerbs == ["seek", "next", "previous", "pause"],
                  "Previous and Pause follow owned Next even when the outgoing recording disallows Previous")
        project(connection, frame(next, position: 0, volume: 0.61, previous: true))
        project(connection, frame(item, device: "external", playing: false, position: 2000), deviceQueue: deviceQueue, upcoming: [next])
        model.next()
        let externalPrevious = connection.recorded().count
        model.previous()
        try check(!model.canGoPrevious && connection.recorded().count == externalPrevious,
                  "An external Next does not override the current device's explicit Previous capability")
        project(connection, frame(next, device: "external", position: 0, previous: true))
        project(connection, frame(item, playing: false, position: 6500, volume: 0.61), deviceQueue: deviceQueue, upcoming: [next, third])
        model.next(); model.next()
        try check(model.displayedTrack?.id == third.id && model.current?.id == item.id,
                  "Rapid Next advances the known preview without jumping back to the old cover")
        project(connection, frame(next, position: 0, volume: 0.61))
        try check(model.displayedTrack?.id == third.id && model.current?.id == next.id && model.isChangingTrack,
                  "The intermediate track observation does not flash over the latest requested cover")
        project(connection, frame(third, position: 0, volume: 0.61))
        try check(!model.isChangingTrack && model.displayedTrack?.id == third.id,
                  "The latest observed skip completes the matching rapid preview")

        let fourth = track("v", title: "Original third upcoming recording")
        func nextBurstIDs(after start: Int) -> Int {
            connection.recorded().dropFirst(start).filter { if case .next = $0 { true } else { false } }.count
        }
        project(connection, firstTrackAt(6500), deviceQueue: deviceQueue, upcoming: [next, next, third])
        let duplicateBurst = connection.recorded().count
        model.next()
        try check(model.displayedTrack?.id == next.id, "The first owned Next can preview the known first queue occurrence")
        model.next(); model.next()
        try check(nextBurstIDs(after: duplicateBurst) == 3 && model.displayedTrack?.id == item.id && !model.canSeek,
                  "Three Next clicks keep all transport writes and stop predicting after a duplicate queue occurrence")
        project(connection, frame(next, position: 0, volume: 0.61), deviceQueue: deviceQueue, upcoming: [next, third])
        try check(model.isChangingTrack && !model.canSeek && model.current?.id == next.id,
                  "An ambiguous duplicate is authoritative for its current cover, while more Next commands remain pending")
        project(connection, frame(third, position: 0, volume: 0.61))
        try check(!model.isChangingTrack && model.displayedTrack?.id == third.id,
                  "The confirmed track takes over after the ambiguous duplicate burst")

        var nearStart = frame(item, playing: false, position: 100, ownedOutputObserved: true)
        nearStart.ownedOutputControl = true
        project(connection, nearStart, deviceQueue: deviceQueue, upcoming: [item, next, third])
        model.next()
        let sameMediaRequest = connection.lastRequestID()
        connection.receive(.commandResult(.init(requestId: sameMediaRequest, error: nil)))
        var sameMedia = firstTrackAt(0)
        project(connection, sameMedia, deviceQueue: deviceQueue, upcoming: [next, third])
        try check(model.isChangingTrack && !model.canSeek,
                  "Next admission, a reset of only 100 ms, and a fresh queue do not establish which occurrence is playing")
        sameMedia.confirmedSameMediaNextRequestId = sameMediaRequest
        connection.receive(.remoteState(sameMedia))
        try check(!model.canSeek, "A same-media confirmation still waits for the playback snapshot")
        project(connection, sameMedia, deviceQueue: deviceQueue, upcoming: [next, third])
        try check(!model.isChangingTrack && model.canSeek && model.current?.id == item.id,
                  "Exact confirmation restores seek for a same-media Next below the 500 ms restart threshold")

        project(connection, firstTrackAt(6500), deviceQueue: deviceQueue, upcoming: [item, item, next])
        model.next()
        let earlierSameMediaRequest = connection.lastRequestID()
        model.next()
        let latestSameMediaRequest = connection.lastRequestID()
        model.togglePlayback()
        guard case .pause = connection.recorded().last else { throw Failure(message: "Pause must follow owned Next") }
        sameMedia.confirmedSameMediaNextRequestId = earlierSameMediaRequest
        project(connection, sameMedia, deviceQueue: deviceQueue, upcoming: [item, next])
        try check(model.isChangingTrack && !model.canSeek,
                  "An earlier confirmed occurrence keeps the newer Next fenced despite a large clock rollback")
        sameMedia.confirmedSameMediaNextRequestId = latestSameMediaRequest
        project(connection, sameMedia, deviceQueue: deviceQueue, upcoming: [next])
        try check(model.canSeek && !model.playbackToggleShowsPause,
                  "Confirmation of the latest Next preserves the trailing Pause intent")
        project(connection, nearStart)

        project(connection, firstTrackAt(100), deviceQueue: deviceQueue, upcoming: [next, next, third])
        model.next()
        model.next()
        var coalesced = frame(next, position: 0, ownedOutputObserved: true)
        coalesced.ownedOutputControl = true
        coalesced.confirmedSameMediaNextRequestId = connection.lastRequestID()
        project(connection, coalesced, deviceQueue: deviceQueue, upcoming: [third])
        try check(!model.isChangingTrack && model.canSeek && model.displayedTrack?.id == next.id,
                  "An exact latest Next can settle a coalesced distinct track and its duplicate")

        project(connection, frame(item, device: "external", playing: false, position: 100))
        model.next()
        var externalSameMedia = frame(item, device: "external", position: 0)
        externalSameMedia.confirmedSameMediaNextRequestId = connection.lastRequestID()
        project(connection, externalSameMedia)
        try check(!model.canSeek, "External playback cannot apply a same-media owned Next confirmation")
        project(connection, frame(next, device: "external", position: 0))

        project(connection, firstTrackAt(6500), deviceQueue: deviceQueue, upcoming: [next])
        model.next(); model.next()
        try check(model.displayedTrack?.id == item.id && !model.canSeek,
                  "An exhausted queue preview does not claim that the intermediate item is the requested second Next")
        project(connection, frame(next, position: 0, volume: 0.61))
        try check(model.isChangingTrack && !model.canSeek, "A known intermediate queue item keeps the Next seek fence")
        project(connection, frame(third, position: 0, volume: 0.61))

        project(connection, firstTrackAt(6500))
        model.next()
        project(connection, firstTrackAt(6500), deviceQueue: deviceQueue, upcoming: [next, third])
        model.next()
        try check(model.displayedTrack?.id == third.id && !model.canSeek,
                  "A fresh queue arriving before the second Next can establish a distinct final preview")
        project(connection, frame(third, position: 0, volume: 0.61))
        try check(!model.isChangingTrack, "The confirmed final track settles both Next clicks when intermediate snapshots coalesce")

        project(connection, firstTrackAt(6500), deviceQueue: deviceQueue, upcoming: [next, third, fourth])
        model.next()
        project(connection, firstTrackAt(6500), deviceQueue: deviceQueue, upcoming: [next, fourth])
        model.next()
        try check(model.displayedTrack?.id == item.id && !model.canSeek,
                  "A contradictory queue suffix cannot predict the final item of an in-flight Next burst")
        project(connection, frame(next, position: 0, volume: 0.61))
        try check(model.isChangingTrack && !model.canSeek, "A common evidenced queue prefix still keeps the Next seek fence")
        project(connection, frame(fourth, position: 0, volume: 0.61))
        project(connection, frame(item, playing: false, position: 6500, volume: 0.61))
        model.startSpotifyTrack(next)
        try check(model.displayedTrack?.id == next.id && model.current?.id == item.id && model.displayedPosition == 0,
                  "Catalog selection changes display immediately before an engine acknowledgement")
        project(connection, frame(next, position: 0, volume: 0.61))
        try check(!model.isChangingTrack && model.current?.id == next.id && model.displayedTrack?.id == next.id,
                  "A matching playback observation hands presentation to the real engine")
        project(connection, frame(next, playing: false, position: 3999, volume: 0.61, previous: true))
        model.previous()
        guard case .skipPrevious = connection.recorded().last else { throw Failure(message: "Previous at 3999 ms must request the earlier track") }
        try check(model.trackTransitionDirection == -1 && model.isChangingTrack, "Previous before 4000 ms selects the backward transition")
        project(connection, frame(item, playing: false, position: 0, volume: 0.61))
        project(connection, frame(item, playing: false, position: 4000, volume: 0.61))
        model.previous()
        guard case .seek(positionMs: 0) = connection.recorded().last else { throw Failure(message: "Previous at 4000 ms must restart the current track") }
        try check(!model.isChangingTrack && model.displayedTrack?.id == item.id, "Previous at the boundary seeks without a track transition")
        model.toggleMute()
        guard case .setVolume(0) = connection.recorded().last else { throw Failure(message: "Mute must submit zero volume") }
        try check(model.isMuted && model.volume == 0, "Mute updates both control surfaces before the engine acknowledges volume")
        project(connection, frame(item, playing: false, position: 0, volume: 0))
        await model.prepareToQuit()
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        let launches = await process.initialVolumes
        try check(launches == [61, 0], "The next Soloist process starts muted using the saved per-output volume")
        var reopenedDriver: FixtureConnection?
        let reopened = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let value = FixtureConnection(receive, items: [item]); reopenedDriver = value; return value
        }, playbackRuntime: runtime, volumeDefaults: defaults)
        project(reopenedDriver!, frame(item, playing: false, volume: 0))
        reopened.toggleMute()
        guard case .setVolume(let restored) = reopenedDriver!.recorded().last else { throw Failure(message: "Unmute must submit the saved audible level") }
        try check(abs(restored - 0.61) < 0.0001 && abs(reopened.volume - 0.61) < 0.0001,
                  "Unmute restores the prior audible volume after reopening the player")
        await reopened.prepareToQuit()
    }

    func rapidQueueModeChecks(_ item: MediaItem) async throws {
        var driver: FixtureConnection?
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            let connection = FixtureConnection(receive, items: [item]); driver = connection; return connection
        })
        let connection = driver!
        project(connection, frame(item))
        model.cycleRepeat(); model.cycleRepeat(); model.cycleRepeat()
        let modes = connection.recorded().compactMap { command -> RepeatMode? in
            if case .setRepeat(let mode) = command { return mode }; return nil
        }
        try check(modes == [.all, .one, .off] && model.repeatMode == RepeatMode.off,
                  "Rapid repeat presses cycle through all three targets before a new transport frame")
        project(connection, frame(item))
        project(connection, frame(item, repeatMode: .all))
        try check(model.repeatMode == RepeatMode.off, "An old Off snapshot and intermediate All do not displace the latest Off")
        project(connection, frame(item))
        project(connection, frame(item, repeatMode: .all))
        try check(model.repeatMode == .all, "Once Off is confirmed a later real mode is displayed normally")

        model.toggleShuffle()
        let obsolete = connection.lastRequestID()
        model.toggleShuffle()
        try check(!model.shuffleEnabled && connection.recorded().compactMap { command -> Bool? in
            if case .setShuffle(let value) = command { return value }; return nil
        } == [true, false], "A double click reverses the pending shuffle setting")
        connection.receive(.commandResult(.init(requestId: obsolete, error: .init(code: "unavailable", message: "Original stale mode failure"))))
        project(connection, frame(item, shuffle: true, repeatMode: .all))
        try check(!model.shuffleEnabled, "An earlier failed shuffle command does not clear the latest target")
        project(connection, frame(item, device: "external-output", shuffle: true, repeatMode: .one))
        try check(model.shuffleEnabled && model.repeatMode == .one, "Queue mode previews are cleared when the playback device changes")
        await model.prepareToQuit()
    }

    func playbackPresentationChecks(_ item: MediaItem) async throws {
        let model = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { [self] receive, _ in
            let driver = FixtureConnection(receive, items: [item], details: .manual)
            connections.append(driver); return driver
        }, lyricsProvider: OriginalLyricsFixture())
        let driver = connections.last!
        project(driver, frame(item, device: "external", playing: false), acknowledge: false)
        // Settle fixture command receipts before observing presentation.
        try await Task.sleep(for: .milliseconds(50))
        func observe(_ count: UIObservationCount) {
            withObservationTracking {
                _ = model.snapshot; _ = model.spotifyState; _ = model.remoteStates
                _ = model.current; _ = model.isPlaying; _ = model.volume
                _ = model.soundSettings; _ = model.audioAnalysis; _ = model.canGoPrevious
                _ = model.shuffleEnabled; _ = model.repeatMode; _ = model.playbackToggleShowsPause
            } onChange: { count.record() }
        }
        let paused = UIObservationCount(), baselineRevision = model.snapshot!.revision
        observe(paused)
        for observation in 1...50 {
            var state = frame(item, device: "external", playing: false)
            state.observationId = UInt64(observation)
            project(driver, state, acknowledge: false)
        }
        try check(paused.count == 0, "50 unchanged paused snapshot/provider heartbeats do not invalidate player presentation")
        try check(model.snapshot!.revision == baselineRevision + 50 && model.spotifyState?.observationId == 50
                  && model.remoteStates["spotify"]?.observationId == 50,
                  "Coalesced presentation keeps the latest raw snapshot and provider observation for commands")
        try check(!driver.recorded().contains { if case .setVolume = $0 { true } else { false } },
                  "Paused presentation coalescing sends no volume command")
        let currentQueueRevision = model.snapshot!.queue.revision
        model.jumpQueue(0)
        try check(driver.recorded().contains { if case .jumpQueue(let revision, _) = $0 { revision == currentQueueRevision } else { false } },
                  "Commands use the newest raw queue revision after coalesced heartbeats")
        try await Task.sleep(for: .milliseconds(30))
        model.send(.setShuffle(true)); model.send(.setRepeat(.all))
        try await Task.sleep(for: .milliseconds(30))
        let pendingModes = UIObservationCount(); observe(pendingModes)
        for _ in 1...5 { project(driver, frame(item, device: "external", playing: false), acknowledge: false) }
        try check(pendingModes.count == 0 && model.shuffleEnabled && model.repeatMode == .all,
                  "Unchanged pending queue modes do not emit nested optional write notifications")
        project(driver, frame(item, device: "external", playing: false, shuffle: true, repeatMode: .all), acknowledge: false)

        for (label, state) in [
            ("paused seek", frame(item, device: "external", playing: false, position: 8_000)),
            ("Play", frame(item, device: "external", position: 8_000)),
            ("volume", frame(item, device: "external", position: 8_000, volume: 0.73)),
            ("queue modes", frame(item, device: "external", position: 8_000, volume: 0.73, shuffle: true, repeatMode: .all)),
            ("playing seek", frame(item, device: "external", position: 25_000, volume: 0.73, shuffle: true, repeatMode: .all))
        ] {
            let changes = UIObservationCount(); observe(changes)
            project(driver, state, acknowledge: false)
            try check(changes.count == 1, "Meaningful \(label) invalidates presentation once")
        }
        let playing = UIObservationCount(); observe(playing)
        for _ in 1...50 {
            project(driver, frame(item, device: "external", position: UInt64(model.positionMilliseconds().rounded()),
                                  volume: 0.73, shuffle: true, repeatMode: .all), acknowledge: false)
        }
        try check(playing.count == 0, "50 continuous playing clocks leave surrounding presentation unchanged")
        let beforeTick = model.positionMilliseconds()
        try await Task.sleep(for: .milliseconds(40))
        try check(model.positionMilliseconds() >= beforeTick + 20,
                  "Timeline reads a fresh raw anchor and continues advancing without whole-player invalidation")
        let updatedItem = MediaItem(id: item.id, title: "Updated original title", artists: item.artists, album: item.album,
                                    durationMs: item.durationMs + 1_000, artwork: item.artwork,
                                    capabilities: item.capabilities, location: item.location)
        for (label, state) in [
            ("metadata and duration", frame(updatedItem, device: "external", position: 25_000)),
            ("capabilities", frame(updatedItem, device: "external", position: 25_000, pauseCapability: false)),
            ("device", frame(updatedItem, device: "changed-external", position: 25_000, pauseCapability: false))
        ] {
            let changes = UIObservationCount(); observe(changes)
            project(driver, state, acknowledge: false)
            try check(changes.count == 1, "Meaningful \(label) refreshes presentation")
        }
        let failed = UIObservationCount(); observe(failed)
        let remote = model.spotifyState!
        driver.receive(.remoteState(RemoteState(provider: remote.provider, connection: remote.connection,
            authenticationRequired: false, issue: PlayerIssue(code: "fixture", message: "Original playback issue"),
            retryAfterSeconds: nil, devices: remote.devices, playback: remote.playback)))
        try check(failed.count == 1 && model.playbackIssue == "Original playback issue",
                  "A provider issue refreshes presentation without requiring a clock change")

        project(driver, frame(item, device: "external", position: 3_950), acknowledge: false)
        let crossing = UIObservationCount(); observe(crossing)
        project(driver, frame(item, device: "external", position: 4_050), acknowledge: false)
        try check(crossing.count == 1 && model.canGoPrevious, "Crossing four seconds refreshes Previous restart availability")

        let request = model.seek(milliseconds: 4_150)
        // Re-arm after the button's pending-command feedback. A matching
        // snapshot must still wake the scrubber inside the drift tolerance.
        let confirmedSeek = UIObservationCount(); observe(confirmedSeek)
        project(driver, frame(item, device: "external", position: 4_150), acknowledge: false)
        try check(request != nil && confirmedSeek.count == 1,
                  "A matching short seek snapshot wakes revision-based scrubber reconciliation")
        try await Task.sleep(for: .milliseconds(30))

        func editable(_ queueRevision: UInt64) -> PlaybackSnapshot {
            PlaybackSnapshot(revision: revision + 1, state: .paused, positionMs: 4_150, volume: 0.73,
                activeProvider: "local", providerConnection: .connected, capabilities: capabilities,
                queue: QueueState(revision: queueRevision, context: .userQueue, current: item, upcoming: [item],
                                  history: [], repeatMode: .off, shuffle: false, playNextCount: 0, forwardCount: 0),
                transport: .local, playbackSessionActive: true,
                session: SessionState(enabled: false, ready: true, restored: false, saving: false, issue: nil), queueIssues: [])
        }
        driver.receive(.snapshot(editable(1)))
        let queueEdit = UIObservationCount(); observe(queueEdit)
        driver.receive(.snapshot(editable(2)))
        try check(queueEdit.count == 1 && model.snapshot?.queue.revision == 2,
                  "Editable queue revision refreshes rendered row admission guards even when rows are unchanged")
        await model.prepareToQuit()
    }

    func run() async throws {
        if CommandLine.arguments.contains("--natural-spatial-retirement-only") {
            try await naturalSpatialRetirementChecks(track())
            return
        }
        if CommandLine.arguments.contains("--spatial-clock-change-only") {
            try await spatialClockChangeChecks(track())
            return
        }
        if CommandLine.arguments.contains("--rapid-spatial-relative-only") {
            try await rapidSpatialRelativeChecks(track())
            try await loadingSpatialContinuationChecks(track())
            return
        }
        try await ArtistBannerRegression.run(check: check)
        let visualSuite = "app.auvra.favorites-render." + UUID().uuidString
        let visualDefaults = UserDefaults(suiteName: visualSuite)!
        defer { visualDefaults.removePersistentDomain(forName: visualSuite) }
        let visualPreferences = SpotifyPreferences(defaults: visualDefaults)
        visualPreferences.setFavorite(RemoteCollection(id: String(repeating: "p",count: 22),title: "Original favorite playlist",url: "https://open.spotify.com/",artwork: nil,kind: .playlist),for: .init(profileID: "original-listener"))
        let process = FixtureProcess()
        let runtime = SoloistRuntime(credentials: FixtureCredentials(),owner: process,deviceName: "Auvra fixture")
        runtime.restore(); try await wait { runtime.status.phase == .ready }
        let player = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { [self] receive, _ in
            let connection = FixtureConnection(receive,items: [track(),track("c",playable: false)]); connections.append(connection); return connection
        },playbackRuntime: runtime,remoteConfirmationTimeout: .milliseconds(200),lyricsProvider: OriginalLyricsFixture(),spotifyPreferences: visualPreferences)
        let driver = connections[0]; let item = track()
        try await playbackPresentationChecks(item)
        try await mainQueryFailureChecks(item)
        try await cursorCatalogNavigationChecks(item)
        try await detailChecks(item)
        try await spokenCatalogChecks(item)
        try await spokenProgressChecks(item)
        try await linkedCatalogChecks(item)
        try await savedMembershipChecks(item)
        try mixedSearchRankingChecks()
        try await restoredAccountDiscoveryCancellationChecks(item)
        try await restoredRemoteChecks(item)
        try await controlResultChecks(item)
        try await transferResultChecks(item)
        try await rapidLocalPlaybackToggles()
        try await localSkipHandoffChecks()
        try await fastSwitchAndVolumeChecks(item)
        try await rapidQueueModeChecks(item)
        try await contextRowStartChecks(item)
        try await ownedStartChecks(item)
        try await lateSelectionPauseChecks(item)
        try await ownedSeekPreviousChecks(item)
        try await chapterOutputChecks()
        try await ownedAnalysisChecks(item)
        try await ownedDeviceOmissionChecks(item)
        try await ownedActivationSelectionChecks(item)
        try await ownedInterruptionChecks(item)
        try await ownedRuntimeRestartChecks(item)
        try await naturalSpatialRetirementChecks(item)
        try await spatialClockChangeChecks(item)
        try await spotifySpatialStereoChecks(item)
        try await rapidSpatialRelativeChecks(item)
        try await loadingSpatialContinuationChecks(item)
        try await pausedSpatialPreviousChecks(item)
        try await outputClockChecks(item)
        try await outputOptionsChecks(item)
        try await playlistPositionChecks(item)
        let firstRun = PlayerStore(loadLibrary: false, loadSession: false, connectionFactory: { receive, _ in
            FixtureConnection(receive, items: [item])
        })
        try await render(SpotifyPlaybackSetup(player: firstRun)
            .padding(24).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .background(AuvraColors.background), name: "light-playback-install", size: NSSize(width: 620, height: 375), colorScheme: .light)
        await firstRun.prepareToQuit()
        for waiting in [false,true] {
            let recovery = SpotifySavedLoginRecovery(retryAvailableAt: waiting ? Date().addingTimeInterval(60) : nil,retry: {},signIn: {})
            try await render(VStack(alignment: .leading,spacing: 18) {
                Text("Account").font(.title2)
                Text("Spotify login could not be reached. Check your connection and try again.").foregroundStyle(.orange)
                recovery
                Text("Your saved login is kept. Retry uses its existing permissions without opening the browser.").font(.caption).foregroundStyle(.secondary)
            }.padding(24).frame(maxWidth: .infinity,maxHeight: .infinity,alignment: .topLeading).background(Color(white: 0.08)).tint(.mint),name: waiting ? "account-recovery-wait" : "account-recovery-ready",size: NSSize(width: 680,height: 260))
        }
        try await listeningPaletteChecks(player)
        try await playbackArtworkChecks()
        try await transportHitTargetChecks(item)
        try await lyricFollowInteractionChecks(item)
        try await favoriteChecks(item)
        try await playlistMetadataChecks(item)
        passed += try await LyricsChecks.run(item: item, directory: evidence.appendingPathComponent("lyrics-fixtures-" + UUID().uuidString, isDirectory: true))
        let session = MediaPlayerSystemSession(player: player); player.attachSystemMedia(session)
        let info = MPNowPlayingInfoCenter.default(); let commands = MPRemoteCommandCenter.shared()
        project(driver,frame(item,playing: false))
        try check(!session.isPublishing, "Paused restore does not claim Now Playing before Play")
        project(driver,frame(item))
        try check(session.isPublishing && info.playbackState == .playing && info.nowPlayingInfo?[MPMediaItemPropertyTitle] as? String == item.title, "Owned output publishes actual track to macOS")
        try check(commands.nextTrackCommand.isEnabled && !commands.previousTrackCommand.isEnabled && !commands.stopCommand.isEnabled, "System controls follow transport capabilities even with an empty queue")
        try check(session.dispatch(.next) == .success && session.dispatch(.stop) == .noActionableNowPlayingItem, "System admission matches supported commands")
        try await wait { driver.recorded().contains { if case .next = $0 { return true }; return false } }
        try check(session.dispatch(.pause) == .success, "System Pause admitted for owned output")
        try await wait { driver.recorded().contains { if case .pause = $0 { return true }; return false } }
        project(driver,frame(item,playing: false))
        try check(session.isPublishing && info.playbackState == .paused, "Pause retains the acquired system session")
        player.transferSpotify("external-output")
        try check(!session.isPublishing && info.nowPlayingInfo == nil, "Device transfer clears macOS ownership immediately")
        project(driver,frame(item,device: "external-output"))
        try check(!session.isPublishing && session.dispatch(.play) == .noActionableNowPlayingItem, "External Connect cannot claim local media ownership")
        player.useAuvraPlayback(); project(driver,frame(item))
        try check(session.isPublishing, "Returning to the owned output reacquires Now Playing")
        let next = track("b",title: "Second original fixture")
        player.startSpotifyTrack(next)
        try check(player.isLoading && player.spotifyStartingItemID == next.id, "Track selection shows pending feedback immediately")
        project(driver,frame(next)); try await wait { player.spotifyStartingItemID == nil }
        try check(!player.isLoading && player.current?.id == next.id, "Pending feedback ends only after actual playback confirmation")
        player.startSpotifyTrack(item); player.send(.pause)
        try check(player.spotifyStartingItemID == nil, "A later transport intent cancels pending selection")
        let original = player.current?.id
        let stale = PlaybackSnapshot(revision: 0,state: .playing,positionMs: 0,volume: 1,activeProvider: "mock",providerConnection: .connected,capabilities: capabilities,queue: QueueState(revision: 0,context: .userQueue,current: item,upcoming: [],history: [],repeatMode: .off,shuffle: false,playNextCount: 0,forwardCount: 0),transport: .local,playbackSessionActive: true,session: SessionState(enabled: false,ready: true,restored: false,saving: false,issue: nil),queueIssues: [])
        driver.receive(.snapshot(stale)); try check(player.current?.id == original, "A stale snapshot cannot replace the playing selection")
        let navigationChecks = AppNavigation()
        navigationChecks.show(.search); navigationChecks.searchText = "Original query"
        navigationChecks.show(.nowPlaying); navigationChecks.leaveNowPlaying()
        try check(navigationChecks.page == .search && navigationChecks.searchText == "Original query", "Listening mode returns to the original page and preserves the search")
        let artist = RemoteCollection(id: String(repeating: "p",count: 22), title: "Original artist", url: "https://open.spotify.com/artist/" + String(repeating: "p",count: 22), artwork: nil, kind: .artist)
        let album = RemoteCollection(id: String(repeating: "a",count: 22), title: "Original album", url: "https://open.spotify.com/album/" + String(repeating: "a",count: 22), artwork: nil, kind: .album)
        navigationChecks.openCollection(artist); navigationChecks.openCollection(album); navigationChecks.leaveCollection()
        try check(navigationChecks.selectedCollection?.id == artist.id, "Discography navigation returns from album to artist")
        navigationChecks.leaveCollection()
        try check(navigationChecks.page == .search, "Collection navigation returns to the original search")
        try check(MacKeyboardShortcuts.routesEscape(keyCode: 53,flags: [],editing: false,modal: false) && !MacKeyboardShortcuts.routesEscape(keyCode: 53,flags: [],editing: true,modal: false) && !MacKeyboardShortcuts.routesEscape(keyCode: 53,flags: [],editing: false,modal: true), "Escape leaves listening mode without stealing text or dialog input")
        player.playSpotifyCollection(album)
        try check(player.isLoading && player.spotifyStartingContextURI == album.uri, "Album selection immediately shows pending transport")
        project(driver,frame(item))
        try check(player.spotifyStartingContextURI == album.uri, "Unrelated playing context cannot confirm album selection")
        project(driver,frame(item,context: album.uri))
        try await wait { player.spotifyStartingContextURI == nil }
        try check(!player.isLoading, "Actual album context confirms collection playback")
        player.saveSpotifyURI(item.id,save: true)
        try await Task.sleep(for: .milliseconds(30))
        try check(player.spotifyEditPending && player.spotifyEditSuccess == nil, "Library admission is not mistaken for a completed network mutation")
        guard case .remote(_, .edit(let operation)) = driver.recorded().last else { throw Failure(message: "Missing typed library command") }
        driver.receive(.remoteEdited(.init(operationId: operation.operationId + 1,collection: nil,error: nil,saved: [])))
        try check(player.spotifyEditPending, "Stale library operation result is ignored")
        driver.receive(.remoteEdited(.init(operationId: operation.operationId,collection: nil,error: PlayerIssue(code: "permission_lost",message: "Original permissions fixture"),saved: [])))
        try check(!player.spotifyEditPending && player.spotifyEditIssue == "Original permissions fixture" && player.spotifyEditSuccess == nil, "A denied mutation preserves failure instead of claiming success")
        player.checkSpotifyURI(item.id)
        guard case .remote(_, .edit(let checkOperation)) = driver.recorded().last else { throw Failure(message: "Missing typed status command") }
        driver.receive(.remoteEdited(.init(operationId: checkOperation.operationId,collection: nil,error: nil,saved: [.init(uri: item.id,saved: true)])))
        try check(player.spotifySaved[item.id] == true && !player.spotifyEditPending, "Saved status is published only from an actual API result")
        player.saveSpotifyURI(item.id,save: false)
        guard case .remote(_, .edit(let removeOperation)) = driver.recorded().last else { throw Failure(message: "Missing typed remove command") }
        try check(player.spotifySaved[item.id] == true, "Remove admission preserves the last confirmed saved status")
        driver.receive(.remoteEdited(.init(operationId: removeOperation.operationId,collection: nil,error: nil,saved: [])))
        try check(player.spotifySaved[item.id] == false, "A confirmed generic library removal updates the visible saved action")
        let navigation = AppNavigation(); let still = StillStore(); let windows = MacWindowService()
        try check(navigation.page == .home && navigation.libraryKind == .playlists && navigation.searchKind == .searchAll,
                  "Navigation starts on Home, with playlists in Your Library and the combined search selected")
        let compact = NSSize(width: 960,height: 650), wide = NSSize(width: 1440,height: 900)
        for (name,page,size,kind): (String,AppPage,NSSize,RemoteCatalogKind?) in [
            ("library-compact", .library, compact, .playlists), ("liked-compact", .likedSongs, compact, .savedTracks),
            ("liked-wide", .likedSongs, wide, .savedTracks), ("search-compact", .search, compact, .searchAll),
            ("queue-compact", .queue, compact, nil), ("now-playing-compact", .nowPlaying, compact, nil), ("local-compact", .localLibrary, compact, nil)
        ] {
            navigation.show(page)
            if page == .search { navigation.searchText = "Original" }
            try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: name,size: size,ready: {
                guard let kind else { return true }
                return player.spotifyQuery.kind == kind && player.spotifyPage?.queryId == player.spotifyQuery.queryId
            })
            if page == .search {
                let result = player.spotifyPage
                try check(player.spotifyQuery.text == "Original"
                          && result?.items.contains { $0.id.hasPrefix("spotify:track:") } == true
                          && result?.items.contains { $0.id.hasPrefix("spotify:episode:") } == true
                          && result?.collections.map(\.entity) == [.album, .artist, .playlist, .show, .audiobook],
                          "Default Search receives songs, episodes and each collection type through one mixed query")
            }
        }
        navigation.show(.home)
        let searchFocusBeforeNewAlbums = navigation.searchFocusRequest
        SpotifyHomeView.showNewAlbums(in: navigation)
        try check(navigation.page == .search && navigation.searchKind == .searchAlbums
                  && navigation.searchText == "tag:new" && navigation.searchFocusRequest == searchFocusBeforeNewAlbums + 1,
                  "New albums opens the existing Search focused on the Albums filter with Spotify's release tag")
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "search-new-albums",size: compact,ready: {
            player.spotifyQuery.kind == .searchAlbums && player.spotifyQuery.text == "tag:new" && player.spotifyPage?.queryId == player.spotifyQuery.queryId
        })
        try check(player.spotifyQuery.offset == 0 && player.spotifyPage?.collections.map(\.entity) == [.album],
                  "New albums forwards the tag verbatim and displays typed album results from the first search page")
        navigation.show(.search); navigation.searchText = "Original"
        for kind in RemoteCatalogKind.searchKinds.dropFirst() {
            navigation.searchKind = kind
            try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "search-\(kind.rawValue)",size: NSSize(width: 960,height: 650),ready: {
                player.spotifyQuery.kind == kind && player.spotifyQuery.text == "Original" && player.spotifyPage?.queryId == player.spotifyQuery.queryId
            })
            let expectedEntity: RemoteEntityKind? = switch kind {
            case .searchAlbums: .album
            case .searchArtists: .artist
            case .searchPlaylists: .playlist
            case .searchShows: .show
            case .searchAudiobooks: .audiobook
            default: nil
            }
            let hasTypedResults: Bool
            if let expectedEntity {
                hasTypedResults = player.spotifyPage?.collections.map(\.entity) == [expectedEntity] && player.spotifyPage?.items.isEmpty == true
            } else {
                let prefix = kind == .searchEpisodes ? "spotify:episode:" : "spotify:track:"
                hasTypedResults = player.spotifyPage?.items.first?.id.hasPrefix(prefix) == true && player.spotifyPage?.collections.isEmpty == true
            }
            try check(navigation.searchText == "Original" && hasTypedResults, "\(kind.rawValue): the filter preserves search text and displays only its own result type")
        }
        navigation.openCollection(artist)
        let beforeArtist = player.spotifyQuery.queryId
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "artist-details",size: NSSize(width: 960,height: 650),ready: {
            player.spotifyDetail(.artist, text: artist.id) != nil && player.spotifyDetail(.artistSelectedTracks, text: artist.id)?.hasResults == true
        })
        try check(player.spotifyQuery.queryId == beforeArtist, "The artist page falls back to selected tracks without replacing the search")
        navigation.openCollection(album)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "album-details",size: NSSize(width: 960,height: 650),ready: { player.spotifyDetail(.album, text: album.id) != nil })
        navigation.show(.nowPlaying)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "listening-wide",size: NSSize(width: 1440,height: 900))
        project(driver, frame(item, playing: false, position: 4450))
        player.lyrics.setVisible(true)
        try await wait { player.lyrics.result != nil }
        guard let document = player.lyrics.result?.document, let start = document.lines.first?.startMs else {
            throw Failure(message: "Missing lyric line start fixture")
        }
        player.seek(milliseconds: player.lyrics.playbackPosition(forLineStart: start))
        guard case .seek(positionMs: 1001) = driver.recorded().last else {
            throw Failure(message: "Lyric line seek must submit the original 1001 ms")
        }
        project(driver, frame(item, playing: false, position: start))
        let pausedMilliseconds = player.positionMilliseconds()
        try check(pausedMilliseconds == 1001 && document.focusedLine(at: pausedMilliseconds + player.lyrics.offsetMilliseconds) == 0,
                  "A paused lyric line seek keeps its exact millisecond and focuses the selected line")
        player.seek(seconds: 1.003)
        guard case .seek(positionMs: 1003) = driver.recorded().last else {
            throw Failure(message: "A seconds-based scrub lost its intended millisecond")
        }
        try check(true, "A seconds-based scrub rounds to the nearest millisecond instead of seeking one millisecond early")
        project(driver, frame(item, playing: false, position: 4450))
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "lyrics-compact",size: NSSize(width: 960,height: 650))
        player.lyrics.setVisible(true)
        try await wait { player.lyrics.result != nil }
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "lyrics-wide",size: NSSize(width: 1440,height: 900))
        try await lyricAppearanceChecks()
        try await lyricsUnavailableChecks(item)
        try await render(VStack(alignment: .leading, spacing: 24) {
            ForEach(Array(OriginalLyricsFixture.typographyLines.enumerated()), id: \.offset) { index, line in
                LyricsLineView(line: line, milliseconds: index == 3 ? 4000 : 3000, active: true, timed: true, reduceMotion: false)
                    .frame(width: index == 3 ? 220 : 580, alignment: .leading)
            }
        }.padding(40).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading).background(.black),name: "lyrics-typography",size: NSSize(width: 960,height: 650))
        let vocals = try LyricsService.parse(OriginalLyricsFixture.overlappingVoices, format: "ttml-amll", duration: 12_000)
        try await render(LyricsVocalGroupView(line: vocals.lines[0], milliseconds: 5500, timed: true, reduceMotion: false)
            .frame(width: 520, alignment: .leading).padding(40)
            .frame(width: 640, height: 280, alignment: .topLeading).background(.black),
            name: "lyrics-backing-vocal", size: NSSize(width: 640, height: 280))
        try check(abs(player.position() - 4.45) < 0.001, "Paused lyrics keep the sustained word at its presented position")
        if #available(macOS 15, *) {
            let locale = Locale(identifier: "en_US")
            player.lyrics.setVisible(true)
            player.lyrics.toggleTranslation(to: locale.language)
            guard let translation = player.lyrics.translationRequest else { throw Failure(message: "Missing native translation fixture request") }
            player.lyrics.acceptTranslation([0: "May this light stay with us", 1: "And gently lead us home",
                                              2: "Through the city and its lights", 3: "We can hear the music within"], for: translation.id)
            try check(player.lyrics.displayedDocument?.focusedLine(at: player.positionMilliseconds()) == 0
                      && player.lyrics.showsTranslation, "A translated native lyrics view follows the original playback position")
            try await render(RootView(player: player,navigation: navigation,still: still,windows: windows).environment(\.locale, locale),
                             name: "lyrics-translation",size: NSSize(width: 960,height: 650))
            player.lyrics.setVisible(true)
            player.lyrics.toggleTranslation(to: locale.language)
        }
        player.lyrics.setVisible(false)
        try await render(MediaItemInspector(player: player,item: item,navigation: navigation),name: "track-inspector",size: NSSize(width: 636,height: 580),ready: { player.spotifyDetail(.track, text: String(repeating: "a",count: 22)) != nil })
        let local = MediaItem(id: "original-local-track", title: "Original local track", artists: ["Original fixture artist"],
                              album: "Original album", durationMs: 180000, artwork: nil, capabilities: capabilities,
                              location: .file(path: "/Fixtures/Original local track.flac"))
        try await render(MediaItemInspector(player: player,item: local,navigation: navigation),name: "light-local-track-inspector",
                         size: NSSize(width: 480,height: 380),colorScheme: .light)
        let longUnavailable = track("l",playable: false,title: String(repeating: "Очень длинное оригинальное название — ",count: 24))
        let longInspector = PlayerStore(loadLibrary: false,loadSession: false,connectionFactory: { receive, _ in FixtureConnection(receive,items: [longUnavailable]) })
        try await render(SpotifyItemInspector(player: longInspector,item: longUnavailable,navigation: navigation),name: "track-inspector-long-unavailable",size: NSSize(width: 636,height: 580),ready: { longInspector.spotifyRelatedCollections(for: longUnavailable).count == 2 })
        await longInspector.prepareToQuit()
        let beforePicker = player.spotifyQuery
        try await render(SpotifyPlaylistPicker(player: player,item: item),name: "playlist-picker",size: NSSize(width: 536,height: 536),ready: { player.spotifyQuery.kind == .playlists && !player.isLoadingSpotifyPage })
        try await wait { player.spotifyQuery.kind == beforePicker.kind && player.spotifyQuery.text == beforePicker.text && !player.isLoadingSpotifyPage }
        try check(player.spotifyQuery.offset == beforePicker.offset && player.spotifyQuery.queryId != beforePicker.queryId, "Closing the playlist picker restores the previous catalog page through a fresh query")
        let replacementPlaylist = RemoteCollection(id: String(repeating: "p",count: 22), title: "Original playlist", url: "https://open.spotify.com/playlist/" + String(repeating: "p",count: 22), artwork: nil)
        try await render(SpotifyPlaylistReplacement(player: player,playlist: replacementPlaylist),name: "playlist-replacement",size: NSSize(width: 596,height: 470))
        try await render(SpotifyPlaylistEditor(player: player,playlist: nil),name: "new-playlist",size: NSSize(width: 516,height: 500))
        try await render(MiniPlayerView(player: player,showMain: {}),name: "mini",size: NSSize(width: 360,height: 264))
        UserDefaults.standard.set("account",forKey: "AuvraSettingsTab")
        try await render(SettingsView(player: player,still: still),name: "account",size: NSSize(width: 720,height: 680),ready: { player.spotifyDetail(.profile) != nil })
        let catalogBeforeHome = player.spotifyQuery.queryId
        navigation.show(.home)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "light-home",size: NSSize(width: 960,height: 650),colorScheme: .light,ready: {
            guard let artist = player.spotifyDetail(.homeTopArtists)?.collections.first else { return false }
            return RemoteCatalogKind.homeKinds.allSatisfy { player.spotifyDetail($0) != nil }
                && player.spotifyDetail(.homeArtistAlbums, text: artist.id)?.collections.first?.title == "Original discovery album"
        })
        try check(player.spotifyQuery.queryId == catalogBeforeHome
                  && player.spotifyDetail(.homeRecent)?.items.first?.id == item.id
                  && player.spotifyDetail(.homeRecent)?.recentPlay(at: 2)?.playedDate != nil
                  && player.spotifyDetail(.homeRadio)?.collections.first?.entity == .playlist,
                  "Home shelves and top-artist album discovery retain the active catalog query and expose original playable results")
        try await render(RootView(player: player, navigation: navigation, still: still, windows: windows)
            .environment(\.windowGlassEnabled, false),
            name: "light-player-bar-solid", size: compact, colorScheme: .light)
        let barLighting = StillStore(compact: true)
        barLighting.setPalette([[0.84, 0.20, 0.18], [0.12, 0.76, 0.35], [0.19, 0.38, 0.87]])
        try await render(RootView(player: player, navigation: navigation, still: still, windows: windows, barLighting: barLighting),
            name: "light-player-bar-frost", size: compact, colorScheme: .light, ready: { barLighting.engine != nil })
        await barLighting.shutdown()
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "home-history",size: NSSize(width: 1060,height: 860))
        navigation.libraryKind = .recent
        navigation.show(.library)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "light-listening-history",size: NSSize(width: 960,height: 650),colorScheme: .light,ready: {
            player.spotifyQuery.kind == .recent && player.spotifyPage?.queryId == player.spotifyQuery.queryId
        })
        guard let history = player.spotifyPage, let firstPlay = history.recentPlay(at: 0), let secondPlay = history.recentPlay(at: 1),
              let firstDate = firstPlay.playedDate, let secondDate = secondPlay.playedDate,
              let playbackContext = secondPlay.contextCollection else { throw Failure(message: "Missing listening history fixtures") }
        try check(history.items[0].id == history.items[1].id && abs(firstDate.timeIntervalSince(secondDate) - 270.25) < 0.001
                  && firstPlay.contextCollection?.entity == .album && playbackContext.entity == .playlist,
                  "Repeated tracks retain distinct dates and playback contexts for each occurrence")
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        let show = String(repeating: "s", count: 22)
        let decoded = try decoder.decode(RecentPlay.self, from: Data("{\"item_index\":2,\"played_at\":\"2026-02-31T16:18:00Z\",\"context_uri\":\"spotify:show:\(show)\"}".utf8))
        let unsupported = RecentPlay(itemIndex: 2, playedAt: "2026-11-05T19:18:00+03:00", contextUri: "https://untrusted.example/playlist/" + show)
        try check(decoded.itemIndex == 2 && decoded.playedDate == nil && decoded.contextCollection?.entity == .show
                  && unsupported.playedDate?.timeIntervalSince(firstDate) == -0.25 && unsupported.contextCollection == nil,
                  "History date and context fields validate independently at the host boundary")
        let ambiguous = RemotePage(recentPlays: [firstPlay, firstPlay], provider: "spotify", queryId: 1, offset: 0, total: 1,
                                   items: [item], collections: [], issue: nil)
        try check(ambiguous.recentPlay(at: 0) == nil && ambiguous.recentPlay(at: 1) == nil, "Ambiguous or out-of-bounds history metadata cannot attach to a row")
        player.spotifyPageBy(1)
        try await wait { player.spotifyPage?.queryId == player.spotifyQuery.queryId }
        try check(player.spotifyQuery.offset == 10 && player.spotifyQuery.text == history.nextCursor && player.spotifyPage?.nextCursor == nil,
                  "Library history advances with Spotify’s cursor while keeping row-local occurrence indexes")
        let startsBeforeContext = driver.recorded().filter { if case .remote(_, .control) = $0 { return true }; return false }.count
        navigation.openCollection(playbackContext)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "history-context",size: NSSize(width: 960,height: 650),ready: {
            player.spotifyDetail(.playlistInfo, text: playbackContext.id)?.collections.first?.id == playbackContext.id
        })
        navigation.leaveCollection()
        try check(navigation.page == .library && navigation.libraryKind == .recent
                  && driver.recorded().filter { if case .remote(_, .control) = $0 { return true }; return false }.count == startsBeforeContext,
                  "Opening a history context uses the native collection route and does not start playback")
        navigation.libraryKind = .playlists
        for (name,page) in [("library",AppPage.library),("liked-songs",.likedSongs),("search",.search),("queue",.queue),("local",.localLibrary)] {
            navigation.show(page)
            if page == .search { navigation.searchText = "Original"; navigation.searchKind = .searchAll }
            try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "light-" + name,size: NSSize(width: 960,height: 650),colorScheme: .light)
        }
        navigation.show(.nowPlaying)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "light-now-playing",size: NSSize(width: 1100,height: 740),colorScheme: .light)
        try await render(RootView(player: player, navigation: navigation, still: still, windows: windows)
            .environment(\.windowGlassEnabled, false),
            name: "light-now-playing-solid", size: NSSize(width: 1100, height: 740), colorScheme: .light)
        player.lyrics.setVisible(true); try await wait { player.lyrics.result != nil }
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "light-lyrics",size: NSSize(width: 1100,height: 740),colorScheme: .light)
        try await render(MiniPlayerView(player: player,showMain: {}),name: "light-mini",size: NSSize(width: 360,height: 264),colorScheme: .light)
        UserDefaults.standard.set("general",forKey: "AuvraSettingsTab")
        try await render(SettingsView(player: player,still: still),name: "light-settings",size: NSSize(width: 540,height: 630),colorScheme: .light)
        UserDefaults.standard.set("privacy", forKey: "AuvraSettingsTab")
        try await render(SettingsView(player: player,still: still),name: "light-privacy-settings",size: NSSize(width: 540,height: 630),colorScheme: .light)
        try await render(PrivacyView(),name: "light-privacy-details",size: NSSize(width: 540,height: 1200),colorScheme: .light)
        try await render(PrivacyView(),name: "privacy-details",size: NSSize(width: 540,height: 1200))
        UserDefaults.standard.set("general", forKey: "AuvraSettingsTab")
        try await render(SpotifyPlaylistEditor(player: player,playlist: nil),name: "light-new-playlist",size: NSSize(width: 516,height: 500),colorScheme: .light)
        let metadataPlaylist = RemoteCollection(id: String(repeating: "p", count: 22), title: "Original playlist · Тихий вечер", url: replacementPlaylist.url,
                                                artwork: nil, isPublic: true, kind: .playlist, description: "Original description — собственный текст для проверки формы.")
        try await render(SpotifyPlaylistEditor(player: player,playlist: metadataPlaylist),name: "light-edit-playlist",size: NSSize(width: 516,height: 500),colorScheme: .light)
        let collaborativePlaylist = RemoteCollection(id: metadataPlaylist.id, title: metadataPlaylist.title, url: metadataPlaylist.url,
                                                     artwork: nil, isPublic: false, kind: .playlist, description: metadataPlaylist.description, collaborative: true)
        try await render(SpotifyPlaylistEditor(player: player,playlist: collaborativePlaylist),name: "collaborative-playlist",size: NSSize(width: 516,height: 500))
        try await render(SpotifyPlaylistEditor(player: player,playlist: collaborativePlaylist),name: "light-collaborative-playlist",size: NSSize(width: 516,height: 500),colorScheme: .light)
        try await render(SpotifyItemInspector(player: player,item: item,navigation: navigation),name: "light-inspector",size: NSSize(width: 636,height: 580),colorScheme: .light)
        let appearancePreference = UserDefaults.standard.object(forKey: AppAppearance.preferenceKey)
        UserDefaults.standard.set("light",forKey: AppAppearance.preferenceKey)
        try check(AppAppearance.current == .light && AppAppearance.current.nativeAppearance?.name == .aqua,"Saved light appearance resolves to the native Aqua appearance")
        UserDefaults.standard.set("invalid-original-fixture",forKey: AppAppearance.preferenceKey)
        try check(AppAppearance.current == .system,"An invalid appearance preference falls back to the system without a reset")
        if let appearancePreference { UserDefaults.standard.set(appearancePreference,forKey: AppAppearance.preferenceKey) } else { UserDefaults.standard.removeObject(forKey: AppAppearance.preferenceKey) }
        for name: NSAppearance.Name in [.aqua,.darkAqua] {
            var ink = 0.0, paper = 0.0
            NSAppearance(named: name)!.performAsCurrentDrawingAppearance {
                func luminance(_ color: NSColor) -> Double {
                    let rgb = color.usingColorSpace(.sRGB)!
                    let channels = [rgb.redComponent,rgb.greenComponent,rgb.blueComponent].map { $0 <= 0.04045 ? $0 / 12.92 : pow(($0 + 0.055) / 1.055,2.4) }
                    return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722
                }
                ink = luminance(NSColor(AuvraColors.accent)); paper = luminance(NSColor(AuvraColors.background))
            }
            try check((max(ink,paper) + 0.05) / (min(ink,paper) + 0.05) >= 4.5,"\(name.rawValue): semantic accent maintains readable text contrast")
        }
        player.errorMessage = "An original failure fixture with a long recovery explanation. Check the selected device and try again."
        try await render(MiniPlayerView(player: player,showMain: {}),name: "mini-error",size: NSSize(width: 360,height: 264))
        navigation.show(.library)
        try await render(RootView(player: player,navigation: navigation,still: still,windows: windows),name: "error-compact",size: NSSize(width: 960,height: 650))
        player.errorMessage = nil
        let delayed = track("d",title: "Delayed confirmation fixture")
        player.startSpotifyTrack(delayed)
        try await wait { player.errorMessage != nil && player.spotifyStartingItemID == nil }
        try check(player.errorMessage?.contains("not been confirmed") == true, "Delayed API confirmation is reported honestly")
        project(driver,frame(delayed))
        try check(player.errorMessage == nil, "Late Playing clears only its own timeout banner")
        player.startSpotifyTrack(item)
        try await wait { player.errorMessage != nil && player.spotifyStartingItemID == nil }
        player.errorMessage = "Unrelated storage fixture error"
        project(driver,frame(item))
        try check(player.errorMessage == "Unrelated storage fixture error", "Late playback preserves an unrelated error")
        player.errorMessage = nil
        await process.exitRuntime(); try await wait { runtime.status.phase == .expired }
        try check(!session.isPublishing, "Owned child exit clears the system session immediately")
        player.saveSpotifyURI(item.id,save: true)
        await player.restartPlayer()
        try check(!player.spotifyEditPending && player.spotifyEditIssue?.contains("before Spotify confirmed") == true && player.spotifyEditSuccess == nil, "Restart does not misreport an unconfirmed library change as successful")
        await player.prepareToQuit(); await still.shutdown()
        try check(!session.isPublishing && info.nowPlayingInfo == nil, "Quit clears native command and metadata ownership")
    }
}
@main
private enum PlayerUIRegression {
    static func main() {
        let app = NSApplication.shared; app.setActivationPolicy(.regular)
        let evidence = URL(fileURLWithPath: CommandLine.arguments[1],isDirectory: true)
        let delegate = Checks(evidence); app.delegate = delegate; app.run()
    }
}
