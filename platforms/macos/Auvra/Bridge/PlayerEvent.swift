import Foundation

enum PlaybackState: String, Codable, Sendable { case idle, loading, playing, paused, stopped, error }
enum ProviderConnection: String, Codable, Sendable { case disconnected, connecting, connected, unavailable, error }
enum RepeatMode: String, Codable, Sendable { case off, all, one }
struct MediaCapabilities: Codable, Sendable { let play, pause, seek, volume, pcmAnalysis, systemMediaSession: Bool; var equalizer: Bool?; var librarySave: Bool? = nil }
struct MediaItem: Codable, Identifiable, Sendable {
    let id: String
    let title: String
    let artists: [String]
    let album: String?
    let durationMs: UInt64
    let artwork: String?
    let capabilities: MediaCapabilities
    let location: MediaLocation?
    var artist: String { artists.isEmpty ? "Unknown artist" : artists.joined(separator: ", ") }
    init(id: String, title: String, artists: [String], album: String?, durationMs: UInt64, artwork: String?, capabilities: MediaCapabilities, location: MediaLocation? = nil) {
        self.id = id; self.title = title; self.artists = artists; self.album = album
        self.durationMs = durationMs; self.artwork = artwork; self.capabilities = capabilities; self.location = location
    }
}
struct QueueState: Decodable, Sendable {
    var deviceQueue: DeviceQueueDetails? = nil
    let revision: UInt64
    let context: PlaybackContext
    let current: MediaItem?
    let upcoming: [MediaItem]
    let history: [String]
    let repeatMode: RepeatMode
    let shuffle: Bool
    let playNextCount, forwardCount: Int
    enum CodingKeys: String, CodingKey { case revision, context, current, upcoming, history, deviceQueue, repeatMode = "repeat", shuffle, playNextCount, forwardCount }
}
struct SessionFileGrant: Codable, Sendable { let originalPath: String; let path: String? }
struct SessionRootGrant: Encodable, Sendable { let id: String; let path: String }
struct SessionAccess: Encodable, Sendable { let roots: [SessionRootGrant]; let files: [SessionFileGrant] }
struct SessionState: Decodable, Sendable { let enabled, ready, restored, saving: Bool; let issue: PlayerIssue? }
struct QueueIssue: Decodable, Sendable { let id: String; let error: PlayerIssue }
struct PlaybackSnapshot: Decodable, Sendable {
    var hostAnalysisStatus: String? = nil
    var hostSpatialStatus: String? = nil
    var hostAnalysisRevokedThrough: UInt64? = nil
    var enhancement: PlaybackEnhancement? = nil
    var playbackRate: Double? = nil
    var restoredRemote: RestoredRemote? = nil
    let revision: UInt64
    let state: PlaybackState
    let positionMs: UInt64
    let volume: Float
    var sound: SoundState?
    var outputStatistics: OutputStatistics?
    let activeProvider: String?
    let providerConnection: ProviderConnection
    let capabilities: MediaCapabilities
    let queue: QueueState
    let transport: TransportControls
    let playbackSessionActive: Bool
    let session: SessionState
    let queueIssues: [QueueIssue]
}
struct RestoredRemote: Decodable, Sendable { let contextUri: String? }
struct PlayerIssue: Decodable, Sendable { let code: String; let message: String }
struct CommandOutcome: Decodable, Sendable { let requestId: UInt64; let error: PlayerIssue? }
struct AudioFeatures: Decodable, Sendable {
    let playbackPositionMs: UInt64
    let rms, subBass, bass, lowMids, mids, presence, highs, transient, spectralCentroidHz: Float
}
struct AudioAnalysisFrame: Decodable, Sendable {
    let generation: UInt64
    let sampleRate, windowFrames, hopFrames: UInt32
    let windowStartFrame, windowEndFrame, droppedFrames: UInt64
    let features: AudioFeatures
    let spectrum: [Float]
}
struct AudioAnalysisObservation: Decodable, Sendable {
    let mediaId: String?
    let frame: AudioAnalysisFrame?
    let modulation: VisualModulation?
}
struct VisualModulation: Decodable, Sendable, Equatable {
    struct Controls: Decodable, Sendable, Equatable {
        let timeSeconds: Double
        let motion, turbulence, light: Float
        var isValid: Bool { timeSeconds.isFinite && timeSeconds >= 0 && [motion, turbulence, light].allSatisfy { $0.isFinite && (0...1).contains($0) } }
    }
    let generation: UInt64
    let sampleRate: UInt32
    let windowEndFrame: UInt64
    let controls: Controls
}
enum PlayerEvent: Decodable, Sendable {
    case audioAnalysis(AudioAnalysisObservation)
    case remoteState(RemoteState), remotePage(RemotePage), remoteEdited(RemoteEditResult)
    case remoteOutputCommand(String, RemoteOutputCommand)
    case libraryState(LibraryState), libraryPage(LibraryPage)
    case snapshot(PlaybackSnapshot), catalog([MediaItem]), error(PlayerIssue), commandResult(CommandOutcome), mediaUpdated(MediaItem)
    private enum Keys: String, CodingKey { case type, state, items, error, requestId, item, page, result, provider, command }
    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: Keys.self)
        switch try values.decode(String.self, forKey: .type) {
        case "audio_analysis": self = .audioAnalysis(try AudioAnalysisObservation(from: decoder))
        case "remote_state": self = .remoteState(try values.decode(RemoteState.self, forKey: .state))
        case "remote_edited": self = .remoteEdited(try values.decode(RemoteEditResult.self, forKey: .result))
        case "remote_output_command": self = .remoteOutputCommand(try values.decode(String.self, forKey: .provider), try values.decode(RemoteOutputCommand.self, forKey: .command))
        case "remote_page": self = .remotePage(try values.decode(RemotePage.self, forKey: .page))
        case "library_state": self = .libraryState(try values.decode(LibraryState.self, forKey: .state))
        case "library_page": self = .libraryPage(try values.decode(LibraryPage.self, forKey: .page))
        case "media_updated": self = .mediaUpdated(try values.decode(MediaItem.self, forKey: .item))
        case "snapshot": self = .snapshot(try values.decode(PlaybackSnapshot.self, forKey: .state))
        case "catalog": self = .catalog(try values.decode([MediaItem].self, forKey: .items))
        case "error": self = .error(try values.decode(PlayerIssue.self, forKey: .error))
        case "command_result": self = .commandResult(CommandOutcome(requestId: try values.decode(UInt64.self, forKey: .requestId), error: try values.decodeIfPresent(PlayerIssue.self, forKey: .error)))
        default: throw DecodingError.dataCorruptedError(forKey: .type, in: values, debugDescription: "Unknown player event")
        }
    }
}

struct EqualizerSettings: Codable, Sendable, Equatable {
    var enabled = false
    var preampDb: Float = 0
    var bandsDb: [Float] = Array(repeating: 0, count: 5)
    var peakProtection = true
}
struct EqualizerPreset: Codable, Sendable, Equatable { var name: String; var equalizer: EqualizerSettings }
struct SoundSettings: Codable, Sendable, Equatable {
    var version: UInt32 = 1
    var equalizer = EqualizerSettings()
    var presets: [EqualizerPreset] = []
}
struct SoundState: Decodable, Sendable {
    let settings: SoundSettings
    let ready, saving: Bool
    let issue: PlayerIssue?
}

struct OutputStatistics: Decodable, Sendable {
    let callbacks, mediaFrames, nonzeroFrames, underrunFrames, analysisDroppedFrames: UInt64
}
