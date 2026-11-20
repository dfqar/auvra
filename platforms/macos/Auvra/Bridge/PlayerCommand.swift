import Foundation

enum PlaybackContext: Codable, Sendable {
    case album(String), playlist(String), search(String), localFolder(String), userQueue
    private enum Keys: String, CodingKey { case kind, id }
    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: Keys.self)
        switch try values.decode(String.self, forKey: .kind) {
        case "album": self = .album(try values.decode(String.self, forKey: .id))
        case "playlist": self = .playlist(try values.decode(String.self, forKey: .id))
        case "search": self = .search(try values.decode(String.self, forKey: .id))
        case "local_folder": self = .localFolder(try values.decode(String.self, forKey: .id))
        case "user_queue": self = .userQueue
        default: throw DecodingError.dataCorruptedError(forKey: .kind, in: values, debugDescription: "Unknown playback context")
        }
    }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: Keys.self)
        switch self {
        case .album(let id): try values.encode("album", forKey: .kind); try values.encode(id, forKey: .id)
        case .playlist(let id): try values.encode("playlist", forKey: .kind); try values.encode(id, forKey: .id)
        case .search(let id): try values.encode("search", forKey: .kind); try values.encode(id, forKey: .id)
        case .localFolder(let id): try values.encode("local_folder", forKey: .kind); try values.encode(id, forKey: .id)
        case .userQueue: try values.encode("user_queue", forKey: .kind)
        }
    }
}

struct HostAnalysisLease: Encodable, Sendable, Equatable {
    let generation: UInt64
    let processId: Int32
    let provider: String
    let deviceId: String
    let mediaId: String
    let spatialStereo: Bool
    var visuals: Bool
    var playing: Bool = true
}
enum PlayerCommand: Encodable, Sendable {
    case ownedOutputInterrupted(provider: String, generation: UInt64, sequence: UInt64, deviceId: String)
    case cancelOwnedOutputRecovery(provider: String)
    case hostAnalysis(HostAnalysisLease?)
    case retireHostAnalysis(generation: UInt64)
    case hostAnalysisAfterSeek(HostAnalysisLease, requestId: UInt64, sequence: UInt64)
    case hostAnalysisAfterClockChange(HostAnalysisLease, retiredGeneration: UInt64, originSequence: UInt64, sequence: UInt64)
    case setSound(SoundSettings), retrySoundStorage, retryPlayback
    case remote(provider: String, action: RemoteAction)
    case initializeSession(directory: String, access: SessionAccess), refreshSessionAccess(SessionAccess)
    case jumpQueue(revision: UInt64, index: Int), removeQueueItem(revision: UInt64, index: Int), clearQueue
    case initializeLibrary(directory: String, roots: [AuthorizedRoot])
    case addLibraryRoot(AuthorizedRoot), removeLibraryRoot(String), scanLibrary(String?), cancelLibraryScan
    case queryLibrary(LibraryQuery), playLibrary(query: LibraryQuery, trackId: String)
    case openFile(path: String)
    case replaceQueue(items: [MediaItem], start: Int, context: PlaybackContext)
    case playNext(MediaItem)
    case play, pause, stop, next, previous, skipPrevious
    case cancelRestoredResume(operationId: UInt64, deviceId: String)
    case pauseConfirmedRestoredResume(operationId: UInt64, deviceId: String)
    case seek(positionMs: UInt64), setVolume(Float), setShuffle(Bool), setRepeat(RepeatMode)
    private enum Keys: String, CodingKey { case lease, generation, requestId, sequence, retiredGeneration, originSequence, type, items, start, context, item, positionMs, volume, enabled, mode, path, directory, roots, root, id, rootId, query, trackId, access, revision, index, provider, action, settings, operationId, deviceId }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: Keys.self)
        switch self {
        case .ownedOutputInterrupted(let provider, let generation, let sequence, let deviceId):
            try values.encode("owned_output_interrupted", forKey: .type)
            try values.encode(provider, forKey: .provider)
            try values.encode(generation, forKey: .generation)
            try values.encode(sequence, forKey: .sequence)
            try values.encode(deviceId, forKey: .deviceId)
        case .cancelOwnedOutputRecovery(let provider):
            try values.encode("cancel_owned_output_recovery", forKey: .type)
            try values.encode(provider, forKey: .provider)
        case .hostAnalysis(let lease): try values.encode("host_analysis", forKey: .type); try values.encodeIfPresent(lease, forKey: .lease)
        case .retireHostAnalysis(let generation):
            try values.encode("retire_host_analysis", forKey: .type)
            try values.encode(generation, forKey: .generation)
        case .hostAnalysisAfterSeek(let lease, let requestId, let sequence):
            try values.encode("host_analysis_after_seek", forKey: .type)
            try values.encode(lease, forKey: .lease)
            try values.encode(requestId, forKey: .requestId)
            try values.encode(sequence, forKey: .sequence)
        case .hostAnalysisAfterClockChange(let lease, let retiredGeneration, let originSequence, let sequence):
            try values.encode("host_analysis_after_clock_change", forKey: .type)
            try values.encode(lease, forKey: .lease)
            try values.encode(retiredGeneration, forKey: .retiredGeneration)
            try values.encode(originSequence, forKey: .originSequence)
            try values.encode(sequence, forKey: .sequence)
        case .setSound(let settings): try values.encode("set_sound", forKey: .type); try values.encode(settings, forKey: .settings)
        case .retrySoundStorage: try values.encode("retry_sound_storage", forKey: .type)
        case .retryPlayback: try values.encode("retry_playback", forKey: .type)
        case .remote(let provider, let action): try values.encode("remote", forKey: .type); try values.encode(provider, forKey: .provider); try values.encode(action, forKey: .action)
        case .initializeSession(let directory, let access):
            try values.encode("initialize_session", forKey: .type); try values.encode(directory, forKey: .directory); try values.encode(access, forKey: .access)
        case .refreshSessionAccess(let access): try values.encode("refresh_session_access", forKey: .type); try values.encode(access, forKey: .access)
        case .jumpQueue(let revision, let index):
            try values.encode("jump_queue", forKey: .type); try values.encode(revision, forKey: .revision); try values.encode(index, forKey: .index)
        case .removeQueueItem(let revision, let index):
            try values.encode("remove_queue_item", forKey: .type); try values.encode(revision, forKey: .revision); try values.encode(index, forKey: .index)
        case .clearQueue: try values.encode("clear_queue", forKey: .type)
        case .initializeLibrary(let directory, let roots):
            try values.encode("initialize_library", forKey: .type); try values.encode(directory, forKey: .directory); try values.encode(roots, forKey: .roots)
        case .addLibraryRoot(let root): try values.encode("add_library_root", forKey: .type); try values.encode(root, forKey: .root)
        case .removeLibraryRoot(let id): try values.encode("remove_library_root", forKey: .type); try values.encode(id, forKey: .id)
        case .scanLibrary(let rootId): try values.encode("scan_library", forKey: .type); try values.encodeIfPresent(rootId, forKey: .rootId)
        case .cancelLibraryScan: try values.encode("cancel_library_scan", forKey: .type)
        case .queryLibrary(let query): try values.encode("query_library", forKey: .type); try values.encode(query, forKey: .query)
        case .playLibrary(let query, let trackId): try values.encode("play_library", forKey: .type); try values.encode(query, forKey: .query); try values.encode(trackId, forKey: .trackId)
        case .openFile(let path): try values.encode("open_file", forKey: .type); try values.encode(path, forKey: .path)
        case .replaceQueue(let items, let start, let context):
            try values.encode("replace_queue", forKey: .type); try values.encode(items, forKey: .items)
            try values.encode(start, forKey: .start); try values.encode(context, forKey: .context)
        case .playNext(let item): try values.encode("play_next", forKey: .type); try values.encode(item, forKey: .item)
        case .play: try values.encode("play", forKey: .type)
        case .pause: try values.encode("pause", forKey: .type)
        case .cancelRestoredResume(let operation, let device):
            try values.encode("cancel_restored_resume", forKey: .type)
            try values.encode(operation, forKey: .operationId)
            try values.encode(device, forKey: .deviceId)
        case .pauseConfirmedRestoredResume(let operation, let device):
            try values.encode("pause_confirmed_restored_resume", forKey: .type)
            try values.encode(operation, forKey: .operationId)
            try values.encode(device, forKey: .deviceId)
        case .stop: try values.encode("stop", forKey: .type)
        case .next: try values.encode("next", forKey: .type)
        case .previous: try values.encode("previous", forKey: .type)
        case .skipPrevious: try values.encode("skip_previous", forKey: .type)
        case .seek(let ms): try values.encode("seek", forKey: .type); try values.encode(ms, forKey: .positionMs)
        case .setVolume(let value): try values.encode("set_volume", forKey: .type); try values.encode(value, forKey: .volume)
        case .setShuffle(let enabled): try values.encode("set_shuffle", forKey: .type); try values.encode(enabled, forKey: .enabled)
        case .setRepeat(let mode): try values.encode("set_repeat", forKey: .type); try values.encode(mode, forKey: .mode)
        }
    }
}
struct CommandRequest: Encodable, Sendable { let requestId: UInt64; let command: PlayerCommand }


enum MediaLocation: Codable, Sendable {
    case file(path: String)
    case remote(uri: String, url: String, service: String)
    var externalURL: URL? { if case .remote(_, let url, _) = self { return URL(string: url) }; return nil }
    var service: String? { if case .remote(_, _, let service) = self { return service }; return nil }
    private enum Keys: String, CodingKey { case kind, path, uri, url, service }
    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: Keys.self)
        switch try values.decode(String.self, forKey: .kind) {
        case "file": self = .file(path: try values.decode(String.self, forKey: .path))
        case "remote": self = .remote(uri: try values.decode(String.self, forKey: .uri), url: try values.decode(String.self, forKey: .url), service: try values.decode(String.self, forKey: .service))
        default:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: values, debugDescription: "Unknown media locator")
        }
    }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: Keys.self)
        switch self {
        case .file(let path): try values.encode("file", forKey: .kind); try values.encode(path, forKey: .path)
        case .remote(let uri, let url, let service): try values.encode("remote", forKey: .kind); try values.encode(uri, forKey: .uri); try values.encode(url, forKey: .url); try values.encode(service, forKey: .service)
        }
    }
}

extension PlayerCommand {
    var invalidatesHostAnalysis: Bool {
        switch self {
        case .play, .pause, .pauseConfirmedRestoredResume, .stop, .next, .previous, .skipPrevious, .seek, .openFile, .replaceQueue, .playLibrary, .jumpQueue, .clearQueue: true
        case .remote(_, let action):
            switch action { case .activate, .disconnect, .credential, .control: true; default: false }
        default: false
        }
    }
}
