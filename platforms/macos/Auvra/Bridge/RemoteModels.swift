import Foundation

struct TransportControls: Decodable, Sendable {
    var queueAdd: Bool? = nil
    var seekForwardMs: UInt64? = nil
    var seekBackwardMs: UInt64? = nil
    let stop, next, previous, shuffle, `repeat`, queueEdit: Bool
    var retry: Bool?
    static let local = Self(stop: true, next: true, previous: true, shuffle: true, repeat: true, queueEdit: true)
}
struct RemoteDevice: Decodable, Identifiable, Sendable {
    let id, name, deviceType: String
    var symbol: String {
        switch deviceType.lowercased() {
        case "computer": "desktopcomputer"
        case "smartphone", "phone": "iphone"
        case "tablet": "ipad"
        case "tv": "tv"
        case "speaker": "hifispeaker"
        default: "airplayaudio"
        }
    }
    let volume: Float?
    let isActive, isRestricted: Bool
}
struct RemotePlayback: Decodable, Sendable {
    var enhancement: PlaybackEnhancement? = nil
    var positionRate: Double? = nil
    var contextUri: String? = nil
    let state: PlaybackState
    let positionMs: UInt64
    let volume: Float
    let current: MediaItem?
    let upcoming: [MediaItem]
    let shuffle: Bool
    let `repeat`: RepeatMode
    let capabilities: MediaCapabilities
    let transport: TransportControls
    let device: RemoteDevice?
}
struct RemoteState: Decodable, Sendable {
    var controlResult: RemoteControlResult? = nil
    var confirmedSameMediaNextRequestId: UInt64? = nil
    var observationId: UInt64? = nil
    var ownedOutputObserved: Bool? = nil
    var ownedOutputControl: Bool? = nil
    let provider: String
    let connection: ProviderConnection
    let authenticationRequired: Bool
    let issue: PlayerIssue?
    let retryAfterSeconds: UInt64?
    let devices: [RemoteDevice]
    let playback: RemotePlayback?
    var observesPlayback: Bool { !authenticationRequired && (connection == .connected || ownedOutputObserved == true) }
    var transportAvailable: Bool { !authenticationRequired && (connection == .connected || ownedOutputControl == true) }
}
struct RemoteControlResult: Decodable, Sendable { let operationId: UInt64; let error: PlayerIssue? }
enum SpotifyPlaybackIdentity {
    /// Chapter metadata uses a chapter ID, while Spotify/Soloist plays its episode URI.
    /// This alias changes no device, process, authorization or generation checks.
    static func matches(_ first: String?, _ second: String?) -> Bool {
        guard let first, let second else { return false }
        if first == second { return true }
        let a = first.split(separator: ":", omittingEmptySubsequences: false)
        let b = second.split(separator: ":", omittingEmptySubsequences: false)
        return a.count == 3 && b.count == 3 && a[0] == "spotify" && b[0] == "spotify"
            && ((a[1] == "chapter" && b[1] == "episode") || (a[1] == "episode" && b[1] == "chapter"))
            && a[2] == b[2] && a[2].utf8.count == 22
            && a[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) })
    }
}
struct RemoteCollection: Decodable, Identifiable, Sendable {
    let id, title, url: String
    let artwork: String?
    var isPublic: Bool? = nil
    private enum CodingKeys: String, CodingKey { case id, title, url, artwork, kind, subtitle, description, ownerId, collaborative, accountId, isPublic = "public" }
    var kind: RemoteEntityKind? = nil
    var subtitle: String? = nil
    var description: String? = nil
    var ownerId: String? = nil
    var collaborative: Bool? = nil
    var accountId: String? = nil
    var entity: RemoteEntityKind { kind ?? .playlist }
    var uri: String { "spotify:\(entity.rawValue):\(id)" }
    var childrenKind: RemoteCatalogKind {
        switch entity { case .playlist: .playlistItems; case .album: .albumTracks; case .artist: .artistAlbums; case .show: .showEpisodes; case .audiobook: .audiobookChapters; case .profile: .profile }
    }
    var detailKind: RemoteCatalogKind {
        switch entity { case .playlist: .playlistInfo; case .album: .album; case .artist: .artist; case .show: .show; case .audiobook: .audiobook; case .profile: .profile }
    }
    var canPlayContext: Bool { [.playlist, .album, .artist].contains(entity) }
}
enum PlaylistEditAccess: Equatable {
    case unknown, readOnly, collaborative, owner
    var canEditItems: Bool { self == .owner || self == .collaborative }
    var canEditDetails: Bool { self == .owner }
}
enum RemoteEntityKind: String, Decodable, Sendable, Hashable {
    case playlist, album, artist, show, audiobook, profile
    var title: String { switch self { case .show: "Podcast"; case .audiobook: "Audiobook"; default: rawValue.capitalized } }
}
struct SpokenProgress: Decodable, Sendable {
    let mediaId: String
    let positionMs: UInt64
    let fullyPlayed: Bool

    var continuePosition: UInt64? { !fullyPlayed && positionMs > 0 ? positionMs : nil }
}
struct RecentPlay: Decodable, Sendable {
    let itemIndex: UInt32
    let playedAt: String?
    let contextUri: String?

    private static let dateFormatters = ["yyyy-MM-dd'T'HH:mm:ss.SSSXXXXX", "yyyy-MM-dd'T'HH:mm:ssXXXXX"].map { format in
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.calendar = Calendar(identifier: .iso8601)
        formatter.timeZone = .gmt
        formatter.isLenient = false
        formatter.dateFormat = format
        return formatter
    }

    var playedDate: Date? {
        guard let playedAt, playedAt.utf8.count <= 40 else { return nil }
        return Self.dateFormatters.lazy.compactMap { $0.date(from: playedAt) }.first
    }

    var contextCollection: RemoteCollection? {
        guard let contextUri, contextUri.utf8.count <= 40, contextUri.hasPrefix("spotify:"),
              let reference = SpotifyContentReference(contextUri) else { return nil }
        let entity: RemoteEntityKind
        switch reference.kind {
        case .album: entity = .album
        case .artist: entity = .artist
        case .playlistInfo: entity = .playlist
        case .show: entity = .show
        default: return nil
        }
        return RemoteCollection(id: reference.id, title: entity.title, url: "https://open.spotify.com/\(entity.rawValue)/\(reference.id)", artwork: nil, kind: entity)
    }
}
struct RemotePage: Decodable, Sendable {
    var positionEdits: Bool? = nil
    var relatedCollections: [RemoteCollection]? = nil
    var detail: Bool? = nil
    var itemPositions: [UInt32]? = nil
    var nextCursor: String? = nil
    var spokenProgress: [SpokenProgress]? = nil
    var recentPlays: [RecentPlay]? = nil
    let provider: String
    let queryId: UInt64
    let offset: UInt32
    let total: UInt64
    let items: [MediaItem]
    let collections: [RemoteCollection]
    let issue: PlayerIssue?
}
extension RemotePage {
    var hasResults: Bool { !items.isEmpty || !collections.isEmpty }
    var hasOmittedResults: Bool { hasResults && issue?.code == "track_unavailable" }

    func recentPlay(at index: Int) -> RecentPlay? {
        guard items.indices.contains(index), (0..<10).contains(index), let recentPlays, recentPlays.count <= 10 else { return nil }
        let matches = recentPlays.filter { $0.itemIndex == UInt32(index) }
        return matches.count == 1 ? matches.first : nil
    }

    func position(at index: Int) -> UInt32? {
        guard items.indices.contains(index) else { return nil }
        if let itemPositions, itemPositions.count == items.count { return itemPositions[index] }
        // Old fixtures/hosts are safe only if no rows were omitted.
        let value = UInt64(offset) + UInt64(index)
        return issue == nil && value < 100_000 ? UInt32(value) : nil
    }
}
enum RemoteCatalogKind: String, Encodable, CaseIterable, Identifiable, Sendable {
    case searchAll = "search_all"
    case search, savedTracks = "saved_tracks", playlists, playlistItems = "playlist_items"
    case searchAlbums = "search_albums", searchArtists = "search_artists", searchPlaylists = "search_playlists", searchShows = "search_shows", searchEpisodes = "search_episodes", searchAudiobooks = "search_audiobooks"
    case savedAlbums = "saved_albums", savedShows = "saved_shows", savedEpisodes = "saved_episodes", savedAudiobooks = "saved_audiobooks", followedArtists = "followed_artists"
    case recent, topTracks = "top_tracks", topArtists = "top_artists"
    case albumTracks = "album_tracks", artistAlbums = "artist_albums", showEpisodes = "show_episodes", audiobookChapters = "audiobook_chapters"
    case artistTopTracks = "artist_top_tracks", artistSelectedTracks = "artist_selected_tracks"
    case playlistInfo = "playlist_info", playlistImages = "playlist_images"
    case album, artist, show, audiobook, track, episode, chapter, profile
    case homeRecent = "home_recent", homeTopTracks = "home_top_tracks", homeTopArtists = "home_top_artists"
    case homePlaylists = "home_playlists", homeRadio = "home_radio"
    case homeArtistAlbums = "home_artist_albums"
    static let searchKinds: [Self] = [.searchAll, .search, .searchAlbums, .searchArtists, .searchPlaylists, .searchShows, .searchEpisodes, .searchAudiobooks]
    static let libraryKinds: [Self] = [.playlists, .savedAlbums, .followedArtists, .savedShows, .savedEpisodes, .savedAudiobooks, .recent, .topTracks, .topArtists]
    static let homeKinds: [Self] = [.homeRecent, .homeTopTracks, .homeRadio, .homeTopArtists, .homePlaylists]
    var isDetail: Bool { Self.homeKinds.contains(self) || [.homeArtistAlbums, .playlistInfo, .playlistImages, .album, .artist, .artistAlbums, .artistTopTracks, .artistSelectedTracks, .show, .audiobook, .track, .episode, .chapter, .profile].contains(self) }
    var isSearch: Bool { Self.searchKinds.contains(self) }
    var usesCursor: Bool { self == .recent || self == .followedArtists }
    var title: String {
        switch self {
        case .searchAll: "All"
        case .search, .savedTracks: "Songs"
        case .searchAlbums, .savedAlbums: "Albums"
        case .searchArtists, .followedArtists: "Artists"
        case .searchPlaylists, .playlists: "Playlists"
        case .searchShows, .savedShows: "Podcasts"
        case .searchEpisodes, .savedEpisodes: "Episodes"
        case .searchAudiobooks, .savedAudiobooks: "Audiobooks"
        case .recent: "Recently played"
        case .topTracks: "Top songs"
        case .topArtists: "Top artists"
        case .homeRecent: "Jump back in"
        case .homeTopTracks: "In your rotation"
        case .homeTopArtists: "Artists for you"
        case .homePlaylists: "From your library"
        case .homeRadio: "Radio playlists"
        default: "Collection"
        }
    }
    var id: Self { self }
}
struct RemoteQuery: Encodable, Sendable {
    let queryId: UInt64
    let kind: RemoteCatalogKind
    let text: String
    let offset: UInt32
    static func boundedText(_ input: String) -> String {
        var result = "", bytes = 0
        for character in input.prefix(256) {
            let length = String(character).utf8.count
            if bytes + length > 512 { break }
            result.append(character)
            bytes += length
        }
        return result
    }
}
struct RemoteContextRow: Sendable {
    let position: UInt32
    let queryId: UInt64
}

enum RemoteControl: Encodable, Sendable {
    case transfer(String, UInt64? = nil), start(MediaItem), startOnDevice(MediaItem, String, UInt64? = nil), enqueue(MediaItem), startContext(String, String, UInt64? = nil)
    case startContextItem(MediaItem, String, String, UInt64, RemoteContextRow)
    case resumeOnDevice(MediaItem, String, UInt64, String?, UInt64? = nil)
    case startSpokenOnDevice(MediaItem, String, UInt64, UInt64)
    case pauseOnDevice(String)
    var operationId: UInt64? {
        switch self {
        case .transfer(_, let id), .startOnDevice(_, _, let id), .startContext(_, _, let id), .resumeOnDevice(_, _, _, _, let id): id
        case .startSpokenOnDevice(_, _, _, let id), .startContextItem(_, _, _, let id, _): id
        default: nil
        }
    }
    private enum Keys: String, CodingKey { case kind, deviceId, item, uri, position, queryId, positionMs, contextUri, operationId }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: Keys.self)
        switch self {
        case .pauseOnDevice(let id): try values.encode("pause_on_device", forKey: .kind); try values.encode(id, forKey: .deviceId)
        case .startSpokenOnDevice(let item, let id, let position, let operation):
            try values.encode("start_spoken_on_device", forKey: .kind)
            try values.encode(item, forKey: .item)
            try values.encode(id, forKey: .deviceId)
            try values.encode(position, forKey: .positionMs)
            try values.encode(operation, forKey: .operationId)
        case .resumeOnDevice(let item, let id, let position, let context, let operation): try values.encode("resume_on_device", forKey: .kind); try values.encode(item, forKey: .item); try values.encode(id, forKey: .deviceId); try values.encode(position, forKey: .positionMs); try values.encodeIfPresent(context, forKey: .contextUri); try values.encodeIfPresent(operation, forKey: .operationId)
        case .startContextItem(let item, let uri, let id, let operation, let row):
            try values.encode("start_context", forKey: .kind)
            try values.encode(item, forKey: .item)
            try values.encode(uri, forKey: .uri)
            try values.encode(id, forKey: .deviceId)
            try values.encode(operation, forKey: .operationId)
            try values.encode(row.position, forKey: .position)
            try values.encode(row.queryId, forKey: .queryId)
        case .startContext(let uri, let id, let operation): try values.encode("start_context", forKey: .kind); try values.encode(uri, forKey: .uri); try values.encode(id, forKey: .deviceId); try values.encodeIfPresent(operation, forKey: .operationId)
        case .transfer(let id, let operation): try values.encode("transfer", forKey: .kind); try values.encode(id, forKey: .deviceId); try values.encodeIfPresent(operation, forKey: .operationId)
        case .start(let item): try values.encode("start", forKey: .kind); try values.encode(item, forKey: .item)
        case .startOnDevice(let item, let id, let operation): try values.encode("start_on_device", forKey: .kind); try values.encode(item, forKey: .item); try values.encode(id, forKey: .deviceId); try values.encodeIfPresent(operation, forKey: .operationId)
        case .enqueue(let item): try values.encode("enqueue", forKey: .kind); try values.encode(item, forKey: .item)
        }
    }
}
enum RemoteAction: Encodable, Sendable {
    case credential(String), disconnect, activate, adoptOutput, refresh, visibility(Bool), query(RemoteQuery), control(RemoteControl), edit(RemoteEditRequest), outputClock(RemoteOutputClock?)
    case outputCommandResult(RemoteOutputCommandResult)
    case outputQueue(RemoteOutputQueue)
    private enum Keys: String, CodingKey { case kind, accessToken, visible, query, control, request, clock, result, queue }
    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: Keys.self)
        switch self {
        case .credential(let token): try values.encode("credential", forKey: .kind); try values.encode(token, forKey: .accessToken)
        case .disconnect: try values.encode("disconnect", forKey: .kind)
        case .activate: try values.encode("activate", forKey: .kind)
        case .adoptOutput: try values.encode("adopt_output", forKey: .kind)
        case .refresh: try values.encode("refresh", forKey: .kind)
        case .visibility(let visible): try values.encode("visibility", forKey: .kind); try values.encode(visible, forKey: .visible)
        case .query(let query): try values.encode("query", forKey: .kind); try values.encode(query, forKey: .query)
        case .edit(let request): try values.encode("edit", forKey: .kind); try values.encode(request, forKey: .request)
        case .control(let control): try values.encode("control", forKey: .kind); try values.encode(control, forKey: .control)
        case .outputClock(let clock): try values.encode("output_clock", forKey: .kind); try values.encode(clock, forKey: .clock)
        case .outputCommandResult(let result): try values.encode("output_command_result", forKey: .kind); try values.encode(result, forKey: .result)
        case .outputQueue(let queue): try values.encode("output_queue", forKey: .kind); try values.encode(queue, forKey: .queue)
        }
    }
}
struct RemoteOutputClock: Encodable, Sendable {
    var volume: Float? = nil
    var options: RemoteOutputOptions? = nil
    var actions: RemoteOutputActions? = nil
    var metadata: RemoteOutputMetadata? = nil
    var controlUptimeMs: UInt64? = nil
    var queueEpoch: UInt64? = nil
    var confirmedSameMediaNext: UInt64? = nil
    let generation, sequence: UInt64
    let deviceId: String
    let mediaId: String?
    let state: PlaybackState
    let positionMs: UInt64
    let rate: Double
    let active: Bool
}
enum QueueEntrySource: String, Codable, Sendable { case context, queue, autoplay }
struct RemoteOutputQueueEntry: Codable, Sendable {
    let mediaId: String
    let source: QueueEntrySource?
    let metadata: RemoteOutputMetadata?
}
struct RemoteOutputQueue: Encodable, Sendable {
    let generation, epoch, sequence: UInt64
    let deviceId, mediaId: String
    let previous, upcoming: [RemoteOutputQueueEntry]
    var autoplayReady: Bool? = nil
}
struct DeviceQueueDetails: Decodable, Sendable {
    let previous: [MediaItem]
    let previousSources: [QueueEntrySource?]?
    let upcomingSources: [QueueEntrySource?]
}
struct RemoteOutputCommand: Decodable, Sendable {
    struct Control: Codable, Sendable {
        enum Kind: String, Codable, Sendable {
            case play, startUri = "start_uri", pause, seek, next, previous, previousFromStart = "previous_from_start"
            case volume, shuffle, `repeat`, enqueue
            case autoplayEnqueue = "autoplay_enqueue"
            var isOutputTransport: Bool { self == .next || self == .previous || self == .previousFromStart || self == .play || self == .pause }
        }
        let kind: Kind
        let positionMs: UInt64?
        let percent: UInt8?
        let enabled: Bool?
        let mode: RepeatMode?
        let uri: String?
        var operationId: UInt64? = nil
    }
    let id, generation, sequence, expiresAtUptimeMs: UInt64
    var requestId: UInt64? = nil
    var requiredQueueEpoch: UInt64? = nil
    var observedQueueEpoch: UInt64? = nil
    var expectedPreviousMediaId: String? = nil
    let deviceId, mediaId: String
    let control: Control
}
struct RemoteOutputCommandResult: Encodable, Sendable {
    let id, generation: UInt64
    let accepted: Bool
}
struct RemoteOutputMetadata: Codable, Sendable {
    let title: String
    let artists: [String]
    let album: String?
    let durationMs: UInt64
    let artwork, contextUri: String?
}
enum PlaybackEnhancement: String, Codable, Sendable { case off, recommendations }
struct RemoteOutputOptions: Codable, Sendable {
    let shuffle: Bool
    let `repeat`: RepeatMode
    let enhancement: PlaybackEnhancement?
}
struct RemoteOutputActions: Codable, Sendable {
    let play, pause, seek, next, previous, shuffle, `repeat`, queueAdd: Bool
    let seekForwardMs, seekBackwardMs: UInt64?
    var isValid: Bool { [seekForwardMs,seekBackwardMs].compactMap { $0 }.allSatisfy { (1...300_000).contains($0) } }
}

struct RemoteEditRequest: Encodable, Sendable { let operationId: UInt64; let edit: RemoteEdit }
struct RemoteEditResult: Decodable, Sendable { let operationId: UInt64; let collection: RemoteCollection?; let error: PlayerIssue?; let saved: [RemoteSavedItem]? }
struct RemoteSavedItem: Decodable, Sendable { let uri: String; let saved: Bool }
struct RemoteEdit: Encodable, Sendable {
    let kind: String
    var jpegBase64: String? = nil
    var uris: [String]? = nil
    var save: Bool? = nil
    var id: String? = nil
    var name: String? = nil
    var description: String? = nil
    var `public`: Bool? = nil
    var collaborative: Bool? = nil
    var from: UInt32? = nil
    var to: UInt32? = nil
    var queryId: UInt64? = nil
    var isValidPlaylistDetailsPatch: Bool {
        guard kind == "playlist_details", let id, SpotifyContentReference("spotify:playlist:\(id)") != nil,
              name != nil || description != nil || self.public != nil || collaborative != nil,
              collaborative != true || self.public == false else { return false }
        if let name, !Self.validPlaylistName(name) { return false }
        return description.map { $0.utf8.count <= 2048 } ?? true
    }
    private static func validPlaylistName(_ name: String) -> Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && name.utf8.count <= 256
            && !name.unicodeScalars.contains(where: { $0.properties.generalCategory == .control })
    }
    var isValidCreatePlaylist: Bool {
        guard kind == "create_playlist", let name, let description, let published = self.public else { return false }
        return Self.validPlaylistName(name) && description.utf8.count <= 2048 && !(published && collaborative == true)
    }
    static func createPlaylist(name: String, description: String, published: Bool, collaborating: Bool) -> Self? {
        let edit = Self(kind: "create_playlist", name: name, description: description, public: published, collaborative: collaborating ? true : nil)
        return edit.isValidCreatePlaylist ? edit : nil
    }
    static func playlistDetailsChanges(_ original: RemoteCollection, name: String, description: String, published: Bool, collaborating: Bool? = nil) -> Self? {
        let effectiveCollaboration = collaborating ?? original.collaborative ?? false
        guard original.entity == .playlist, !(published && effectiveCollaboration) else { return nil }
        var edit = Self(kind: "playlist_details", id: original.id,
                        name: name == original.title ? nil : name,
                        description: description == (original.description ?? "") ? nil : description,
                        public: published == (original.isPublic ?? false) ? nil : published,
                        collaborative: collaborating.flatMap { $0 == (original.collaborative ?? false) ? nil : $0 })
        // These coupled settings are committed together, even if the peer
        // setting changed in another client after this form opened.
        if edit.collaborative == true { edit.public = false }
        if edit.public == true { edit.collaborative = false }
        return edit.isValidPlaylistDetailsPatch ? edit : nil
    }
}

struct SpotifyContentReference {
    let kind: RemoteCatalogKind
    let id: String
    var uri: String { "spotify:\(kind == .playlistInfo ? "playlist" : kind.rawValue):\(id)" }

    init?(_ text: String) {
        let parts: [Substring]
        if text.hasPrefix("spotify:") {
            parts = Array(text.split(separator: ":", omittingEmptySubsequences: false).dropFirst())
        } else {
            guard let url = URLComponents(string: text), url.scheme == "https", url.host == "open.spotify.com",
                  url.user == nil, url.password == nil, url.port == nil, url.fragment == nil else { return nil }
            let path = url.path.split(separator: "/")
            parts = path.first?.hasPrefix("intl-") == true ? Array(path.dropFirst()) : path
        }
        guard parts.count == 2, parts[1].utf8.count == 22,
              parts[1].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) else { return nil }
        switch parts[0] {
        case "track": kind = .track
        case "episode": kind = .episode
        case "chapter": kind = .chapter
        case "album": kind = .album
        case "artist": kind = .artist
        case "playlist": kind = .playlistInfo
        case "show": kind = .show
        case "audiobook": kind = .audiobook
        default: return nil
        }
        id = String(parts[1])
    }
}

/// A replacement is one explicit operation. Preserve order and duplicate occurrences.
struct PlaylistReplacementDraft {
    let uris: [String]
    let issue: String?
    init(_ input: String) {
        guard input.utf8.count <= 32 * 1024 else { uris = []; issue = "Use at most 100 track or episode links."; return }
        let lines = input.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
        guard lines.count <= 100 else { uris = []; issue = "A replacement can contain up to 100 items."; return }
        var parsed: [String] = []
        for (index, line) in lines.enumerated() {
            guard let uri = Self.uri(line) else { uris = []; issue = "Line \(index + 1): use a Spotify track or episode link or URI."; return }
            parsed.append(uri)
        }
        uris = parsed; issue = nil
    }
    static func uri(_ text: String) -> String? {
        guard let reference = SpotifyContentReference(text), [.track, .episode].contains(reference.kind) else { return nil }
        return reference.uri
    }
}
