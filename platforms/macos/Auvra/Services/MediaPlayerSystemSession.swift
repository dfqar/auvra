import AppKit
import Foundation
import MediaPlayer
import OSLog

/// Values parsed from a native event before crossing to MainActor.
enum SystemMediaAction: Sendable {
    case play, pause, toggle, stop, next, previous
    case seek(Double), skip(Double), shuffle(Bool), repeatMode(RepeatMode)
    var kind: SystemMediaCommand {
        switch self {
        case .play: .play; case .pause: .pause; case .toggle: .toggle
        case .stop: .stop; case .next: .next; case .previous: .previous
        case .seek, .skip: .seek; case .shuffle: .shuffle; case .repeatMode: .repeatMode
        }
    }
    var isValid: Bool {
        switch self { case .seek(let value), .skip(let value): value.isFinite; default: true }
    }
}
enum SystemMediaCommand: Hashable, Sendable { case play, pause, toggle, stop, next, previous, seek, shuffle, repeatMode }

struct SystemMediaProjection: Equatable, Sendable {
    struct Metadata: Equatable, Sendable {
        let id, title, artist: String
        let album, artwork: String?
        let duration: Double
        let rate: Double
        let forwardInterval, backwardInterval: Double
        let playing, shuffle: Bool
        let repeatMode: RepeatMode
        let commands: Set<SystemMediaCommand>
    }
    let metadata: Metadata
    let position: Double
    init?(_ state: PlaybackSnapshot, ownsOutput: Bool = false) {
        guard state.playbackSessionActive, state.capabilities.systemMediaSession || ownsOutput,
              state.providerConnection == .connected || ownsOutput,
              let item = state.queue.current, item.durationMs > 0,
              state.state == .playing || (state.state == .paused && (state.capabilities.play || ownsOutput)) else { return nil }
        var commands: Set<SystemMediaCommand> = []
        if state.transport.stop { commands.insert(.stop) }
        if state.transport.shuffle { commands.insert(.shuffle) }
        if state.transport.repeat { commands.insert(.repeatMode) }
        if state.capabilities.play { commands.insert(.play) }
        if state.capabilities.pause { commands.insert(.pause) }
        if state.state == .playing ? state.capabilities.pause : state.capabilities.play { commands.insert(.toggle) }
        if state.capabilities.seek { commands.insert(.seek) }
        if state.transport.next { commands.insert(.next) }
        if state.positionMs >= PlayerStore.previousRestartMilliseconds ? state.capabilities.seek : state.transport.previous {
            commands.insert(.previous)
        }
        metadata = Metadata(id: item.id, title: item.title, artist: item.artist, album: item.album,
                            artwork: item.artwork, duration: Double(item.durationMs) / 1000,
                            rate: state.playbackRate.flatMap { $0.isFinite && (0...4).contains($0) ? $0 : nil } ?? 1,
                            forwardInterval: state.transport.seekForwardMs.flatMap { (1...300_000).contains($0) ? Double($0) / 1000 : nil } ?? 10,
                            backwardInterval: state.transport.seekBackwardMs.flatMap { (1...300_000).contains($0) ? Double($0) / 1000 : nil } ?? 10,
                            playing: state.state == .playing, shuffle: state.queue.shuffle,
                            repeatMode: state.queue.repeatMode, commands: commands)
        position = min(Double(state.positionMs) / 1000, metadata.duration)
    }
}

/// The OS may call command handlers from any queue. No UI or FFI is touched under this lock.
private final class MediaCommandGate: @unchecked Sendable {
    struct Ticket: Sendable { let generation: UInt64; let id: String }
    private let lock = NSLock()
    private var generation: UInt64 = 0
    private var id: String?
    private var commands: Set<SystemMediaCommand> = []
    func update(_ projection: SystemMediaProjection?) {
        lock.lock(); defer { lock.unlock() }
        if id != projection?.metadata.id { generation &+= 1 }
        id = projection?.metadata.id; commands = projection?.metadata.commands ?? []
    }
    func admit(_ action: SystemMediaAction) -> Ticket? {
        lock.lock(); defer { lock.unlock() }
        guard action.isValid, commands.contains(action.kind), let id else { return nil }
        return Ticket(generation: generation, id: id)
    }
    func accepts(_ ticket: Ticket, action: SystemMediaAction) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return id == ticket.id && generation == ticket.generation && commands.contains(action.kind)
    }
}

/// Read-only NSImage ownership for the system's artwork request callback.
private final class FrozenMediaArtwork: @unchecked Sendable {
    let image: NSImage
    init(_ image: NSImage) { self.image = image }
    // MediaPlayer encodes artwork on its own access queue. Create this callback
    // outside MainActor so it cannot inherit an actor assertion from publish().
    func makeArtwork() -> MPMediaItemArtwork {
        MPMediaItemArtwork(boundsSize: image.size) { [self] _ in image }
    }
}

@MainActor
final class MediaPlayerSystemSession: SystemMediaSession {
    private static let tracePlayback = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
    private static let playbackTiming = Logger(subsystem: "app.auvra.player", category: "PlaybackTiming")
    private weak var player: PlayerStore?
    private let infoCenter: MPNowPlayingInfoCenter
    private let commandCenter: MPRemoteCommandCenter
    nonisolated private let gate = MediaCommandGate()
    private var targets: [(MPRemoteCommand, Any)] = []
    private var current: SystemMediaProjection?
    private var published: SystemMediaProjection?
    private var publishedAt = 0.0
    private var artwork: MPMediaItemArtwork?
    private var artworkKey: String?
    private var artworkTask: Task<Void, Never>?
    private(set) var isPublishing = false

    init(player: PlayerStore, infoCenter: MPNowPlayingInfoCenter = .default(), commandCenter: MPRemoteCommandCenter = .shared()) {
        self.player = player; self.infoCenter = infoCenter; self.commandCenter = commandCenter
    }
    func publish(_ state: PlaybackSnapshot) {
        let began = Self.tracePlayback ? ProcessInfo.processInfo.systemUptime : nil
        defer {
            if let began {
                let elapsed = (ProcessInfo.processInfo.systemUptime - began) * 1000
                if elapsed >= 8 {
                    Self.playbackTiming.info("system_media_publish elapsed_ms=\(elapsed, privacy: .public)")
                }
            }
        }
        guard let projection = projection(state) else { clear(); return }
        let previous = current?.metadata
        let metadata = projection.metadata
        current = projection
        if previous?.id != metadata.id || previous?.commands != metadata.commands { gate.update(projection) }
        if targets.isEmpty { registerCommands() }
        configure(metadata, previous: previous)
        let key = metadata.id + "\n" + (metadata.artwork ?? "")
        if artworkKey != key {
            artworkTask?.cancel(); artwork = nil; artworkKey = key
            if let reference = projection.metadata.artwork {
                artworkTask = Task { [weak self] in
                    guard let image = await LocalArtworkLoader.shared.image(for: reference), !Task.isCancelled,
                          let self, artworkKey == key, current != nil else { return }
                    let frozen = FrozenMediaArtwork(image)
                    artwork = frozen.makeArtwork()
                    writeInfo()
                }
            }
        }
        let now = ProcessInfo.processInfo.systemUptime
        let expected = (published?.position ?? 0) + (published?.metadata.playing == true ? (now - publishedAt) * (published?.metadata.rate ?? 1) : 0)
        if published?.metadata != projection.metadata || abs(projection.position - expected) > 0.15 || (projection.metadata.playing && now - publishedAt >= 5) { writeInfo() }
    }
    private func writeInfo() {
        guard let current else { return }
        let metadata = current.metadata
        var info: [String: Any] = [
            MPMediaItemPropertyTitle: metadata.title, MPMediaItemPropertyArtist: metadata.artist,
            MPMediaItemPropertyPlaybackDuration: metadata.duration,
            MPNowPlayingInfoPropertyElapsedPlaybackTime: current.position,
            MPNowPlayingInfoPropertyPlaybackRate: metadata.playing ? metadata.rate : 0.0,
            MPNowPlayingInfoPropertyDefaultPlaybackRate: 1.0,
            MPNowPlayingInfoPropertyMediaType: MPNowPlayingInfoMediaType.audio.rawValue,
            MPNowPlayingInfoPropertyIsLiveStream: false,
            MPNowPlayingInfoPropertyExternalContentIdentifier: metadata.id
        ]
        if let album = metadata.album { info[MPMediaItemPropertyAlbumTitle] = album }
        if let artwork { info[MPMediaItemPropertyArtwork] = artwork }
        infoCenter.nowPlayingInfo = info
        if !isPublishing || published?.metadata.playing != metadata.playing {
            infoCenter.playbackState = metadata.playing ? .playing : .paused
        }
        isPublishing = true; published = current; publishedAt = ProcessInfo.processInfo.systemUptime
    }
    func clear() {
        gate.update(nil); current = nil; published = nil
        artworkTask?.cancel(); artworkTask = nil; artworkKey = nil; artwork = nil
        for (command, token) in targets { command.isEnabled = false; command.removeTarget(token) }
        targets.removeAll()
        if isPublishing { infoCenter.playbackState = .stopped; infoCenter.nowPlayingInfo = nil }
        isPublishing = false
    }
    /// Native handlers and the integration smoke enter the identical admission path.
    nonisolated func dispatch(_ action: SystemMediaAction) -> MPRemoteCommandHandlerStatus {
        guard let ticket = gate.admit(action) else { return .noActionableNowPlayingItem }
        DispatchQueue.main.async { [weak self] in
            guard let self, gate.accepts(ticket, action: action), let player,
                  let state = player.snapshot, state.queue.current?.id == ticket.id,
                  projection(state)?.metadata.commands.contains(action.kind) == true else { return }
            switch action {
            case .play: player.send(.play)
            case .pause: player.send(.pause)
            case .toggle: player.togglePlayback()
            case .stop: player.stop()
            case .next: player.next()
            case .previous: player.previous()
            case .seek(let seconds): player.seek(seconds: seconds)
            case .skip(let seconds): player.seekRelative(seconds: seconds)
            case .shuffle(let enabled): player.send(.setShuffle(enabled))
            case .repeatMode(let mode): player.send(.setRepeat(mode))
            }
        }
        return .success
    }
    private func projection(_ state: PlaybackSnapshot) -> SystemMediaProjection? {
        SystemMediaProjection(state, ownsOutput: player?.ownsSystemOutput(state) == true)
    }
    private func register(_ command: MPRemoteCommand, _ parse: @escaping @Sendable (MPRemoteCommandEvent) -> SystemMediaAction?) {
        let token = command.addTarget(handler: Self.commandHandler(session: self, parse: parse))
        targets.append((command, token))
    }
    nonisolated private static func commandHandler(session: MediaPlayerSystemSession, parse: @escaping @Sendable (MPRemoteCommandEvent) -> SystemMediaAction?) -> @Sendable (MPRemoteCommandEvent) -> MPRemoteCommandHandlerStatus {
        { [weak session] event in
            guard let action = parse(event) else { return .commandFailed }
            return session?.dispatch(action) ?? .noActionableNowPlayingItem
        }
    }
    private func registerCommands() {
        register(commandCenter.playCommand) { _ in .play }
        register(commandCenter.pauseCommand) { _ in .pause }
        register(commandCenter.togglePlayPauseCommand) { _ in .toggle }
        register(commandCenter.stopCommand) { _ in .stop }
        register(commandCenter.nextTrackCommand) { _ in .next }
        register(commandCenter.previousTrackCommand) { _ in .previous }
        register(commandCenter.changePlaybackPositionCommand) { event in
            (event as? MPChangePlaybackPositionCommandEvent).map { .seek($0.positionTime) }
        }
        register(commandCenter.skipForwardCommand) { event in (event as? MPSkipIntervalCommandEvent).map { .skip($0.interval) } }
        register(commandCenter.skipBackwardCommand) { event in (event as? MPSkipIntervalCommandEvent).map { .skip(-$0.interval) } }
        register(commandCenter.changeShuffleModeCommand) { event in
            guard let event = event as? MPChangeShuffleModeCommandEvent else { return nil }
            switch event.shuffleType { case .off: return .shuffle(false); case .items: return .shuffle(true); default: return nil }
        }
        register(commandCenter.changeRepeatModeCommand) { event in
            guard let event = event as? MPChangeRepeatModeCommandEvent else { return nil }
            switch event.repeatType { case .off: return .repeatMode(.off); case .one: return .repeatMode(.one); case .all: return .repeatMode(.all); default: return nil }
        }
    }
    private func configure(_ metadata: SystemMediaProjection.Metadata, previous: SystemMediaProjection.Metadata?) {
        if previous?.forwardInterval != metadata.forwardInterval {
            commandCenter.skipForwardCommand.preferredIntervals = [NSNumber(value: metadata.forwardInterval)]
        }
        if previous?.backwardInterval != metadata.backwardInterval {
            commandCenter.skipBackwardCommand.preferredIntervals = [NSNumber(value: metadata.backwardInterval)]
        }
        if previous?.commands != metadata.commands {
            let commands: [(MPRemoteCommand, SystemMediaCommand)] = [
                (commandCenter.playCommand, .play), (commandCenter.pauseCommand, .pause),
                (commandCenter.togglePlayPauseCommand, .toggle), (commandCenter.stopCommand, .stop),
                (commandCenter.nextTrackCommand, .next), (commandCenter.previousTrackCommand, .previous),
                (commandCenter.changePlaybackPositionCommand, .seek), (commandCenter.skipForwardCommand, .seek),
                (commandCenter.skipBackwardCommand, .seek), (commandCenter.changeShuffleModeCommand, .shuffle),
                (commandCenter.changeRepeatModeCommand, .repeatMode)
            ]
            for (command, kind) in commands {
                let enabled = metadata.commands.contains(kind)
                if previous?.commands.contains(kind) != enabled { command.isEnabled = enabled }
            }
        }
        if previous?.shuffle != metadata.shuffle {
            commandCenter.changeShuffleModeCommand.currentShuffleType = metadata.shuffle ? .items : .off
        }
        if previous?.repeatMode != metadata.repeatMode {
            commandCenter.changeRepeatModeCommand.currentRepeatType = switch metadata.repeatMode { case .off: .off; case .one: .one; case .all: .all }
        }
    }
}
