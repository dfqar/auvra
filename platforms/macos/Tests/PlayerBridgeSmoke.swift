import Foundation

@main
struct PlayerBridgeSmoke {
    @MainActor
    static func waitFor(_ label: String, condition: @MainActor () -> Bool, player: PlayerStore, allowingError: Bool = false) async throws {
        for _ in 0..<100 {
            if let message = player.errorMessage, !allowingError { throw SmokeFailure(message: message) }
            if condition() { return }
            try await Task.sleep(for: .milliseconds(30))
        }
        throw SmokeFailure(message: "Timed out: \(label)")
    }
    @MainActor
    static func main() async throws {
        let player = PlayerStore(loadLibrary: false)
        try await waitFor("catalog",condition: { player.tracks.count == 3 && player.snapshot != nil },player: player)
        player.play(player.tracks[0])
        try await waitFor("play",condition: { player.isPlaying && player.current?.id == player.tracks[0].id },player: player)
        try await Task.sleep(for: .milliseconds(350))
        guard player.position() > 0 else { throw SmokeFailure(message: "Clock did not advance") }
        // Hold a seek preview while real Rust snapshots keep advancing.
        let scrubber = PlaybackScrubber()
        scrubber.begin(at: player.position(), trackID: player.current?.id)
        guard scrubber.update(80) == nil else { throw SmokeFailure(message: "Drag emitted an early seek") }
        let beforeDragRevision = player.snapshot?.revision ?? 0
        try await waitFor("clock during drag", condition: { (player.snapshot?.revision ?? 0) > beforeDragRevision }, player: player)
        scrubber.reconcile(player.snapshot)
        guard scrubber.isEditing, scrubber.position == 80, player.position() < 10 else {
            throw SmokeFailure(message: "Backend updates overwrote a held seek or caused an early seek")
        }
        scrubber.end(seek: player.seek(seconds:))
        try await waitFor("seek acknowledgement", condition: { (player.snapshot?.positionMs ?? 0) >= 80000 }, player: player)
        scrubber.reconcile(player.snapshot)
        guard scrubber.position == nil else { throw SmokeFailure(message: "Seek preview did not release after acknowledgement") }
        player.togglePlayback()
        try await waitFor("pause",condition: { player.snapshot?.state == .paused },player: player)
        let paused = player.position()
        try await Task.sleep(for: .milliseconds(300))
        guard player.position() == paused else { throw SmokeFailure(message: "Paused position drifted") }
        player.seek(seconds: 10)
        try await waitFor("seek",condition: { player.snapshot?.positionMs == 10000 },player: player)
        scrubber.begin(at: player.position(), trackID: player.current?.id)
        _ = scrubber.update(100)
        player.next()
        try await waitFor("next",condition: { player.current?.id == player.tracks[1].id },player: player)
        scrubber.reconcile(player.snapshot)
        guard !scrubber.isEditing, scrubber.position == nil else {
            throw SmokeFailure(message: "A cancelled drag could seek the next track")
        }
        player.toggleShuffle()
        try await waitFor("shuffle",condition: { player.snapshot?.queue.shuffle == true },player: player)
        player.cycleRepeat()
        try await waitFor("repeat",condition: { player.snapshot?.queue.repeatMode == .all },player: player)
        player.playNext(player.tracks[0])
        try await waitFor("insert",condition: { player.snapshot?.queue.upcoming.first?.id == player.tracks[0].id },player: player)
        player.next()
        try await waitFor("play-next",condition: { player.current?.id == player.tracks[0].id },player: player)
        player.previous()
        try await waitFor("previous",condition: { player.current?.id == player.tracks[1].id },player: player)
        try await waitFor("all initial receipts", condition: { player.pendingCommands.isEmpty }, player: player)
        guard player.snapshot?.providerConnection == .connected,
              player.snapshot?.capabilities.pcmAnalysis == false,
              player.snapshot?.capabilities.systemMediaSession == false else {
            throw SmokeFailure(message: "The fake provider advertised real audio")
        }
        player.stop()
        try await waitFor("stop", condition: { player.snapshot?.state == .stopped && player.pendingCommands.isEmpty }, player: player)
        guard player.snapshot?.positionMs == 0 else { throw SmokeFailure(message: "Stop retained position") }
        let restart = player.send(.play)
        try await waitFor("restart receipt", condition: { player.lastCommandOutcome?.requestId == restart && player.isPlaying }, player: player)
        let previousVolume = player.volume
        let rejection = player.send(.setVolume(2))
        try await waitFor("rejection receipt", condition: { player.lastCommandOutcome?.requestId == rejection }, player: player, allowingError: true)
        guard player.lastCommandOutcome?.error?.code == "invalid_command", player.volume == previousVolume,
              player.isPlaying, player.pendingCommands.isEmpty else {
            throw SmokeFailure(message: "Rejected command changed state or left a pending receipt")
        }
        player.errorMessage = nil
        let resumeItem = MediaItem(id: "spotify:track:" + String(repeating: "a",count: 22),title: "Original saved selection ABI check",artists: ["Original fixture author"],album: nil,durationMs: 8000,artwork: nil,
            capabilities: .init(play: true,pause: true,seek: true,volume: true,pcmAnalysis: false,systemMediaSession: false),location: .remote(uri: "spotify:track:" + String(repeating: "a",count: 22),url: "https://open.spotify.com/",service: "Spotify"))
        let resumeReceipt = player.send(.remote(provider: "spotify",action: .control(.resumeOnDevice(resumeItem,"original-fixture-output",3456,nil))))
        try await waitFor("saved selection admission rejection",condition: { player.lastCommandOutcome?.requestId == resumeReceipt },player: player,allowingError: true)
        guard player.lastCommandOutcome?.error?.message.contains("saved selection") == true && player.isPlaying else { throw SmokeFailure(message: "Saved selection wire command changed an unrelated local session") }
        player.errorMessage = nil
        let actions = RemoteOutputActions(play: true,pause: false,seek: true,next: false,previous: false,shuffle: true,repeat: true,queueAdd: true,seekForwardMs: 15000,seekBackwardMs: 30000)
        let clock = RemoteOutputClock(volume: 0.4,options: RemoteOutputOptions(shuffle: true,repeat: .all,enhancement: .recommendations),actions: actions,generation: 1,sequence: 1,deviceId: "original-fixture-output",mediaId: resumeItem.id,state: .playing,positionMs: 2000,rate: 1.5,active: true)
        let clockReceipt = player.send(.remote(provider: "spotify",action: .outputClock(clock)))
        try await waitFor("owned clock wire admission",condition: { player.lastCommandOutcome?.requestId == clockReceipt },player: player)
        guard player.lastCommandOutcome?.error == nil && player.isPlaying && player.current?.id.hasPrefix("mock:") == true && player.snapshot?.playbackRate == 1 else { throw SmokeFailure(message: "An unbound owned observation activated playback or changed the local clock") }
        let clearClockReceipt = player.send(.remote(provider: "spotify",action: .outputClock(nil)))
        try await waitFor("owned clock clear admission",condition: { player.lastCommandOutcome?.requestId == clearClockReceipt },player: player)
        _ = player.send(.setVolume(0.2))
        let lastInBurst = player.send(.setVolume(0.4))
        try await waitFor("ordered burst", condition: { player.pendingCommands.isEmpty && player.lastCommandOutcome?.requestId == lastInBurst && abs(player.volume - 0.4) < 0.001 }, player: player)
        let first = player.tracks[0], second = player.tracks[1]
        let short = MediaItem(id: first.id, title: first.title, artists: first.artists, album: first.album, durationMs: 150, artwork: nil, capabilities: first.capabilities)
        let ending = MediaItem(id: second.id, title: second.title, artists: second.artists, album: second.album, durationMs: 800, artwork: nil, capabilities: second.capabilities)
        player.send(.setRepeat(.off))
        player.send(.setShuffle(false))
        let contextRequest = player.send(.replaceQueue(items: [short, ending], start: 0, context: .search("quiet")))
        try await waitFor("context receipt", condition: { player.lastCommandOutcome?.requestId == contextRequest }, player: player)
        try await waitFor("automatic next", condition: { player.current?.id == second.id && player.current?.durationMs == 800 }, player: player)
        guard case .search("quiet") = player.snapshot?.queue.context else { throw SmokeFailure(message: "Playback context was not decoded") }
        try await waitFor("queue completion", condition: { player.snapshot?.state == .stopped && player.pendingCommands.isEmpty }, player: player)
        guard player.snapshot?.positionMs == 800 else { throw SmokeFailure(message: "Completion lost the final position") }
        for _ in 0..<12 {
            var temporary: PlayerStore? = PlayerStore(loadLibrary: false)
            try await waitFor("temporary owner", condition: { temporary?.tracks.count == 3 }, player: temporary!)
            weak var released = temporary
            temporary = nil
            guard released == nil else { throw SmokeFailure(message: "PlayerStore retained a bridge cycle") }
            released = nil
        }
        if let directory = CommandLine.arguments.dropFirst().first {
            let root = URL(fileURLWithPath: directory, isDirectory: true)
            player.setVolume(0.3)
            player.openFile(root.appendingPathComponent("Auvra Quiet Check.wav"))
            try await waitFor("real WAV", condition: { player.isPlaying && player.current?.durationMs == 8000 && player.current?.capabilities.pcmAnalysis == true && player.pendingCommands.isEmpty }, player: player)
            guard player.current?.location != nil, player.localTracks.count == 1, player.demoTracks.count == 3 else { throw SmokeFailure(message: "Local locator/metadata update was lost") }
            try await waitFor("sample clock", condition: { (player.snapshot?.positionMs ?? 0) > 200 }, player: player)
            try await waitFor("real audio analysis", condition: {
                guard let f = player.audioAnalysis else { return false }
                return f.features.rms > 0.002 && abs(f.features.spectralCentroidHz - 440) < 5
            }, player: player)
            let analysisGeneration = player.audioAnalysis!.generation
            guard let modulation = player.visualModulation, modulation.generation == analysisGeneration,
                  modulation.controls.turbulence > 0.1 else { throw SmokeFailure(message: "Measured audio did not reach ModulationBus/Swift") }
            let f = player.audioAnalysis!
            guard f.spectrum.count == 32, f.windowEndFrame - f.windowStartFrame == UInt64(f.windowFrames),
                  f.features.playbackPositionMs == f.windowEndFrame * 1000 / UInt64(f.sampleRate) else { throw SmokeFailure(message: "Invalid compact analysis DTO") }
            var observed: Set<UInt64> = []
            let analysisUntil = ProcessInfo.processInfo.systemUptime + 1
            while ProcessInfo.processInfo.systemUptime < analysisUntil {
                if let frame = player.audioAnalysis { observed.insert(frame.windowEndFrame) }
                try await Task.sleep(for: .milliseconds(5))
            }
            guard observed.count >= 22 && observed.count <= 60 else { throw SmokeFailure(message: "Analysis bridge cadence: \(observed.count)") }
            print("M7 Swift projection: \(observed.count) observations/s, spectrum 32, sample timestamp, RMS \(f.features.rms), centroid \(f.features.spectralCentroidHz) Hz, drops \(f.droppedFrames)")
            player.togglePlayback()
            try await waitFor("audio pause", condition: { player.snapshot?.state == .paused && player.pendingCommands.isEmpty }, player: player)
            guard player.audioAnalysis == nil && player.visualModulation == nil else { throw SmokeFailure(message: "Paused analysis/modulation leaked") }
            let position = player.snapshot?.positionMs
            try await Task.sleep(for: .milliseconds(400))
            guard player.snapshot?.positionMs == position else { throw SmokeFailure(message: "Sample clock advanced on pause") }
            scrubber.begin(at: player.position(), trackID: player.current?.id)
            _ = scrubber.update(6)
            scrubber.end(seek: player.seek(seconds:))
            scrubber.supersede(with: player.latestPositionRequest)
            guard scrubber.position == 6 else { throw SmokeFailure(message: "Accepted seek did not keep its own preview") }
            player.previous()
            scrubber.supersede(with: player.latestPositionRequest)
            guard scrubber.position == nil else { throw SmokeFailure(message: "Previous left an obsolete seek preview") }
            try await waitFor("Previous immediately after real seek", condition: {
                player.pendingCommands.isEmpty && player.snapshot?.state == .paused && player.snapshot?.positionMs == 0
            }, player: player)
            player.seek(seconds: 4)
            try await waitFor("paused audio seek", condition: { player.snapshot?.state == .paused && player.snapshot?.positionMs == 4000 && player.pendingCommands.isEmpty }, player: player)
            player.togglePlayback()
            try await waitFor("audio resume", condition: { player.isPlaying && (player.snapshot?.positionMs ?? 0) > 4100 }, player: player)
            try await waitFor("fresh analysis after seek", condition: {
                guard let frame = player.audioAnalysis else { return false }
                return frame.generation != analysisGeneration && frame.features.playbackPositionMs >= 4100
            }, player: player)
            player.openFile(root.appendingPathComponent("Auvra Quiet Check.flac"))
            try await waitFor("real FLAC", condition: { player.isPlaying && player.current?.title == "Auvra Quiet Check" && player.localTracks.count == 2 && player.pendingCommands.isEmpty }, player: player)
            player.seek(seconds: 7.7)
            try await waitFor("audio EOF", condition: { player.snapshot?.state == .stopped && player.snapshot?.positionMs == 8000 && player.pendingCommands.isEmpty }, player: player)
            guard player.audioAnalysis == nil && player.visualModulation == nil else { throw SmokeFailure(message: "EOF analysis/modulation leaked") }
            player.openFile(root.appendingPathComponent("missing.wav"))
            try await waitFor("async missing file", condition: { player.snapshot?.state == .error && player.errorMessage != nil && player.pendingCommands.isEmpty }, player: player, allowingError: true)
            guard player.audioAnalysis == nil && player.visualModulation == nil else { throw SmokeFailure(message: "Error analysis/modulation leaked") }
            player.errorMessage = nil
            player.openFile(root.appendingPathComponent("Auvra Mono Check.wav"))
            try await waitFor("audio recovery", condition: { player.isPlaying && player.current?.durationMs == 3000 && player.pendingCommands.isEmpty }, player: player)
            player.stop()
            try await waitFor("audio stop", condition: { player.snapshot?.state == .stopped && player.snapshot?.positionMs == 0 && player.pendingCommands.isEmpty }, player: player)
            guard player.audioAnalysis == nil && player.visualModulation == nil else { throw SmokeFailure(message: "Stopped analysis/modulation leaked") }
            print("M2 Swift → Rust → CoreAudio: WAV/FLAC, locator/metadata, sample clock, pause/seek/resume, EOF, missing-file error and mono recovery passed")
        }
        print("Swift ↔ Rust ABI v2: typed commands/events, correlated success/rejection, ordered burst, stop/restart, automatic next/completion, queue context, held seek and 12 lifecycle cycles passed")
    }
}
struct SmokeFailure: Error { let message: String }
