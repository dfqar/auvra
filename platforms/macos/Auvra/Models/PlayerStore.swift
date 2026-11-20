import Foundation
import Observation
import OSLog

/// The relative-command proof uses continuous time. The ordinary capture cooldown
/// uses awake uptime, independently of the SDK clocks used to verify playback.
struct SpatialIntentTimeSource {
    let continuousNow: () -> ContinuousClock.Instant
    let uptimeNow: () -> TimeInterval
}

/// Presentation equality never replaces the raw coordinator/provider state.
/// Revisions, receipt IDs, statistics and clock heartbeats still reach transport;
/// only changes a view can present invalidate its Observation dependencies.
private enum PlayerPresentation {
    static func optional<T>(_ lhs: T?, _ rhs: T?, equal: (T, T) -> Bool) -> Bool {
        switch (lhs, rhs) {
        case (.none, .none): true
        case let (.some(lhs), .some(rhs)): equal(lhs, rhs)
        default: false
        }
    }
    static func issue(_ a: PlayerIssue?, _ b: PlayerIssue?) -> Bool {
        optional(a, b) { $0.code == $1.code && $0.message == $1.message }
    }
    static func capabilities(_ a: MediaCapabilities, _ b: MediaCapabilities) -> Bool {
        a.play == b.play && a.pause == b.pause && a.seek == b.seek && a.volume == b.volume
            && a.pcmAnalysis == b.pcmAnalysis && a.systemMediaSession == b.systemMediaSession
            && a.equalizer == b.equalizer && a.librarySave == b.librarySave
    }
    static func transport(_ a: TransportControls, _ b: TransportControls) -> Bool {
        a.stop == b.stop && a.next == b.next && a.previous == b.previous && a.shuffle == b.shuffle
            && a.repeat == b.repeat && a.queueEdit == b.queueEdit && a.retry == b.retry
            && a.queueAdd == b.queueAdd && a.seekForwardMs == b.seekForwardMs && a.seekBackwardMs == b.seekBackwardMs
    }
    static func location(_ a: MediaLocation, _ b: MediaLocation) -> Bool {
        switch (a, b) {
        case let (.file(a), .file(b)): a == b
        case let (.remote(au, al, aservice), .remote(bu, bl, bservice)): au == bu && al == bl && aservice == bservice
        default: false
        }
    }
    static func media(_ a: MediaItem, _ b: MediaItem) -> Bool {
        a.id == b.id && a.title == b.title && a.artists == b.artists && a.album == b.album
            && a.durationMs == b.durationMs && a.artwork == b.artwork && capabilities(a.capabilities, b.capabilities)
            && optional(a.location, b.location, equal: location)
    }
    static func context(_ a: PlaybackContext, _ b: PlaybackContext) -> Bool {
        switch (a, b) {
        case (.userQueue, .userQueue): true
        case let (.album(a), .album(b)), let (.playlist(a), .playlist(b)),
             let (.search(a), .search(b)), let (.localFolder(a), .localFolder(b)): a == b
        default: false
        }
    }
    static func queue(_ a: QueueState, _ b: QueueState) -> Bool {
        context(a.context, b.context) && optional(a.current, b.current, equal: media)
            && a.upcoming.elementsEqual(b.upcoming, by: media) && a.history == b.history
            && a.repeatMode == b.repeatMode && a.shuffle == b.shuffle
            && a.playNextCount == b.playNextCount && a.forwardCount == b.forwardCount
            && optional(a.deviceQueue, b.deviceQueue, equal: { a, b in
                a.previous.elementsEqual(b.previous, by: media)
                    && a.previousSources == b.previousSources && a.upcomingSources == b.upcomingSources
            })
    }
    static func snapshot(_ lhs: PlaybackSnapshot?, _ rhs: PlaybackSnapshot?, anchoredAt: Double, now: Double) -> Bool {
        optional(lhs, rhs) { a, b in
            guard a.state == b.state, a.volume == b.volume, a.playbackRate == b.playbackRate,
                  a.activeProvider == b.activeProvider, a.providerConnection == b.providerConnection,
                  a.playbackSessionActive == b.playbackSessionActive, a.enhancement == b.enhancement,
                  a.hostAnalysisStatus == b.hostAnalysisStatus, a.hostSpatialStatus == b.hostSpatialStatus,
                  optional(a.restoredRemote, b.restoredRemote, equal: { $0.contextUri == $1.contextUri }),
                  capabilities(a.capabilities, b.capabilities), transport(a.transport, b.transport), queue(a.queue, b.queue),
                  !a.transport.queueEdit || a.queue.revision == b.queue.revision
            else { return false }
            guard a.session.enabled == b.session.enabled, a.session.ready == b.session.ready,
                  a.session.restored == b.session.restored, a.session.saving == b.session.saving,
                  issue(a.session.issue, b.session.issue),
                  a.queueIssues.elementsEqual(b.queueIssues, by: { $0.id == $1.id && issue($0.error, $1.error) }),
                  optional(a.sound, b.sound, equal: { a, b in
                      a.settings == b.settings && a.ready == b.ready && a.saving == b.saving && issue(a.issue, b.issue)
                  }) else { return false }
            guard a.state == .playing, a.providerConnection == .connected || a.playbackSessionActive else {
                return a.positionMs == b.positionMs
            }
            // Previous becomes restart-current at four seconds. Its button is
            // outside TimelineView and needs this one semantic crossing.
            guard (a.positionMs >= 4_000) == (b.positionMs >= 4_000) else { return false }
            let duration = a.queue.current?.durationMs ?? 0
            let rate = a.playbackRate.flatMap { $0.isFinite && (0...4).contains($0) ? $0 : nil } ?? 1
            let expected = min(Double(duration > 0 ? duration : 86_400_000),
                               Double(a.positionMs) + max(0, now - anchoredAt) * rate * 1000)
            // TimelineView reads the refreshed raw anchor on its own tick. A
            // seek/discontinuity still invalidates immediately, including paused seeks.
            return abs(Double(b.positionMs) - expected) <= 250
        }
    }
    static func device(_ a: RemoteDevice, _ b: RemoteDevice) -> Bool {
        a.id == b.id && a.name == b.name && a.deviceType == b.deviceType && a.volume == b.volume
            && a.isActive == b.isActive && a.isRestricted == b.isRestricted
    }
    static func remote(_ lhs: RemoteState?, _ rhs: RemoteState?) -> Bool {
        optional(lhs, rhs) { a, b in
            a.provider == b.provider && a.connection == b.connection && a.authenticationRequired == b.authenticationRequired
                && a.ownedOutputObserved == b.ownedOutputObserved && a.ownedOutputControl == b.ownedOutputControl
                && a.confirmedSameMediaNextRequestId == b.confirmedSameMediaNextRequestId
                && issue(a.issue, b.issue) && a.retryAfterSeconds == b.retryAfterSeconds
                && a.devices.elementsEqual(b.devices, by: device)
                && optional(a.playback, b.playback, equal: { a, b in
                    a.state == b.state && (a.state == .playing || a.positionMs == b.positionMs)
                        && a.volume == b.volume && a.positionRate == b.positionRate && a.contextUri == b.contextUri
                        && a.enhancement == b.enhancement && a.shuffle == b.shuffle && a.repeat == b.repeat
                        && optional(a.current, b.current, equal: media) && a.upcoming.elementsEqual(b.upcoming, by: media)
                        && capabilities(a.capabilities, b.capabilities) && transport(a.transport, b.transport)
                        && optional(a.device, b.device, equal: device)
                })
        }
    }
}

@MainActor @Observable
final class PlayerStore {
    private static let playbackTiming = Logger(subsystem: "app.auvra.player", category: "PlaybackTiming")
    private static let tracePlayback = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
    @ObservationIgnored private var latestSnapshot: PlaybackSnapshot?
    @ObservationIgnored private var presentationSeek: (provider: String?, media: String, position: UInt64, deadline: Double)?
    private var snapshotPresentationRevision: UInt64 = 0
    private(set) var snapshot: PlaybackSnapshot? {
        get { _ = snapshotPresentationRevision; return latestSnapshot }
        set {
            let now = ProcessInfo.processInfo.systemUptime
            var changed = !PlayerPresentation.snapshot(latestSnapshot, newValue, anchoredAt: positionAnchorTime, now: now)
            if let seek = presentationSeek {
                if let state = newValue, state.activeProvider == seek.provider, state.queue.current?.id == seek.media {
                    if abs(Double(state.positionMs) - Double(seek.position)) < 750 || state.state == .error {
                        // Scrubbers reconcile via snapshot.revision, including a
                        // short seek within the continuous-clock drift tolerance.
                        changed = true; presentationSeek = nil
                    } else if now > seek.deadline { presentationSeek = nil }
                } else { presentationSeek = nil }
            }
            // Keep every raw revision/position for commands and TimelineView's
            // next tick, even when it does not invalidate the surrounding UI.
            latestSnapshot = newValue; positionAnchorTime = now
            if changed { snapshotPresentationRevision &+= 1 }
        }
    }
    private(set) var analysisObservation: AudioAnalysisObservation?
    private(set) var tracks: [MediaItem] = []
    private(set) var pendingCommands: Set<UInt64> = []
    @ObservationIgnored private var pendingOutputUpdates: Set<UInt64> = []
    private(set) var latestPositionRequest: UInt64?
    private struct OwnedPreviousIntent {
        let request, bridge, credentials, runtime: UInt64
        let processID: Int32
        let deviceID: String
        let account: SpotifyPreferences.AccountKey
        let deadline: ContinuousClock.Instant
    }
    @ObservationIgnored private var ownedPreviousIntent: OwnedPreviousIntent?
    private struct LocalSeekIntent {
        var request: UInt64
        let provider, mediaID: String
        let resolvesPreviousInEngine: Bool
    }
    private var localLoadingSeekIntent: LocalSeekIntent?
    private struct PendingOwnedSeek {
        let request, positionMs, generation, sequence: UInt64
        let mediaID, deviceID: String
        let queueEpoch: UInt64?
        let deadline: Double
    }
    private var pendingOwnedSeek: PendingOwnedSeek?
    private struct SpatialSeekConfirmation {
        let seek: PendingOwnedSeek
        let original: SoloistClock
        let processID: Int32
        let contextURI: String?
        var dispatch: (id: UInt64, sequence: UInt64, uptime: Double, wallMs: Double)?
        var accepted = false
    }
    @ObservationIgnored private var spatialSeekConfirmation: SpatialSeekConfirmation?
    private struct SpatialPreviousConfirmation {
        var request: UInt64 = 0
        let runtime, bridge, originSequence, originQueueEpoch: UInt64
        let processID: Int32
        let deviceID, originID, contextURI: String
        var dispatch: (id: UInt64, sequence: UInt64, uptime: Double, predecessor: String)?
        var accepted = false
    }
    @ObservationIgnored private var spatialPreviousConfirmation: SpatialPreviousConfirmation?
    private struct SpatialRelativeConfirmation {
        let bridge, runtime, credentials: UInt64
        let account: SpotifyPreferences.AccountKey
        let processID: Int32
        let deviceID, originID, contextURI: String
        let originSequence, originEpoch: UInt64
        let deadline: ContinuousClock.Instant
        var request: UInt64 = 0
        var kind: RemoteOutputCommand.Control.Kind = .next
        var continued = false
        var dispatch: (id: UInt64, sequence: UInt64, epoch: UInt64, uptime: Double, target: String)?
        var accepted = false
        var diagnosticMask: UInt64?
    }
    @ObservationIgnored private var spatialRelativeConfirmation: SpatialRelativeConfirmation?
    private struct PendingLocalPlayback {
        let request: UInt64
        let provider, mediaID: String
        let play: Bool
    }
    private var pendingLocalPlayback: PendingLocalPlayback?
    private(set) var lastCommandOutcome: CommandOutcome?
    var errorMessage: String?
    private(set) var isChoosingFile = false
    private(set) var bridgeFailed = false
    private(set) var isRestarting = false
    private var soundDraft: SoundSettings?
    @ObservationIgnored private var soundRequest: UInt64?
    @ObservationIgnored private var bridgeEpoch: UInt64 = 0
    @ObservationIgnored private var initialRestoreTask: Task<Void, Never>?
    @ObservationIgnored private var isClosing = false
    @ObservationIgnored private let loadsLibrary: Bool
    @ObservationIgnored private let loadsSpotify: Bool
    private(set) var isChoosingFolder = false
    private(set) var libraryState: LibraryState?
    private(set) var libraryPage: LibraryPage?
    private(set) var libraryQuery = LibraryQuery(queryId: 0)
    private(set) var libraryAlbum: LibraryAlbum?
    var spotifyClientID: String {
        didSet { UserDefaults.standard.set(spotifyClientID, forKey: "AuvraSpotifyClientID") }
    }
    private(set) var spotifyAccount = SpotifyAccountStatus()
    let lyrics: LyricsStore
    let spotifyPlayback: SoloistRuntime
    private(set) var isChoosingSpotifyRuntime = false
    private(set) var selectedSpotifyDeviceID: String?
    private struct PendingSpotifyTransfer {
        let deviceID: String
        let selection: String?
        let operation: UInt64
        let request: UInt64
        let observation: UInt64?
        var accepted = false
    }
    @ObservationIgnored private var pendingSpotifyTransfer: PendingSpotifyTransfer?
    @ObservationIgnored private var spotifyTransferTimeout: Task<Void, Never>?
    private(set) var favoritePlaylist: RemoteCollection?
    var favoritePlaylistID: String? { favoritePlaylist?.id }
    var favoritePlaylistTitle: String? { favoritePlaylist?.title }
    @ObservationIgnored private let spotifyPreferences: SpotifyPreferences
    @ObservationIgnored private let volumeDefaults: UserDefaults
    private struct PendingVolume {
        let request: UInt64
        let value: Double
        let provider, device: String?
    }
    private var pendingVolume: PendingVolume?
    @ObservationIgnored private var pendingVolumeTimeout: Task<Void, Never>?
    private enum QueueModeKind: Hashable { case shuffle, repeatMode }
    private struct PendingQueueMode<Value: Equatable> {
        let request: UInt64
        let value: Value
        let scope: QueueModeScope
        var sawDifferent: Bool

        mutating func reconcile(_ confirmed: Value, scope: QueueModeScope?) -> Bool {
            guard self.scope == scope else { return true }
            if value != confirmed { sawDifferent = true }
            return sawDifferent && value == confirmed
        }
    }
    private struct QueueModeScope: Equatable {
        let provider, device, context: String?
    }
    private var pendingShuffle: PendingQueueMode<Bool>?
    private var pendingRepeat: PendingQueueMode<RepeatMode>?
    @ObservationIgnored private var queueModeTimeouts: [QueueModeKind: Task<Void, Never>] = [:]
    private static let audibleVolumeKey = "AuvraLastAudibleVolume"
    @ObservationIgnored private var latestRemoteStates: [String: RemoteState] = [:]
    private var remotePresentationRevision: UInt64 = 0
    private(set) var remoteStates: [String: RemoteState] {
        get { _ = remotePresentationRevision; return latestRemoteStates }
        set {
            let changed = latestRemoteStates.count != newValue.count || !newValue.allSatisfy {
                PlayerPresentation.remote(latestRemoteStates[$0.key], $0.value)
            }
            latestRemoteStates = newValue
            if changed { remotePresentationRevision &+= 1 }
        }
    }
    @ObservationIgnored private var latestSpotifyState: RemoteState?
    private var spotifyPresentationRevision: UInt64 = 0
    private(set) var spotifyState: RemoteState? {
        get { _ = spotifyPresentationRevision; return latestSpotifyState }
        set {
            let changed = !PlayerPresentation.remote(latestSpotifyState, newValue)
            latestSpotifyState = newValue
            if changed { spotifyPresentationRevision &+= 1 }
        }
    }
    private(set) var spotifySaved: [String: Bool] = [:]
    @ObservationIgnored private var spotifySavedOrder: [String] = []
    @ObservationIgnored private var spotifySavedPageFloor: [String: UInt64] = [:]
    private(set) var spotifyEditPending = false
    private(set) var spotifyEditIssue: String?
    private(set) var spotifyEditSuccess: String?
    private(set) var spotifyCreatedPlaylist: RemoteCollection?
    @ObservationIgnored private var spotifyOperationID: UInt64 = 0
    @ObservationIgnored private var spotifySubmittedEdit: RemoteEdit?
    @ObservationIgnored private var spotifyEditTicket: UInt64?
    private(set) var spotifyDetails: [String: RemotePage] = [:]
    private(set) var spotifyDetailQueries: [UInt64: RemoteQuery] = [:]
    private(set) var spotifyDetailEpoch: UInt64 = 0
    private(set) var homePlaylistRevision: UInt64 = 0
    @ObservationIgnored private var spotifyDetailOrder: [String] = []
    @ObservationIgnored private var spotifyReadRevoked = false
    @ObservationIgnored private var spotifyDetailTickets: [UInt64: UInt64] = [:]
    @ObservationIgnored private var spotifyMainQueryTickets: [UInt64: UInt64] = [:]
    private struct SpotifyCursorPage: Equatable {
        let value: String
        let offset: UInt32
        static let first = SpotifyCursorPage(value: "", offset: 0)
    }
    @ObservationIgnored private var spotifyCursorPages: [RemoteCatalogKind: [SpotifyCursorPage]] = [:]
    private(set) var spotifyPage: RemotePage?
    private(set) var spotifyQuery = RemoteQuery(queryId: 0, kind: .savedTracks, text: "", offset: 0)
    @ObservationIgnored private var spotifyAuth: SpotifyAuthorization?
    @ObservationIgnored private var spotifyCredentialEpoch: UInt64 = 0
    @ObservationIgnored private let spotifyAuthorizationFactory: SpotifyAuthorizationFactory
    @ObservationIgnored private var spotifyQueryID: UInt64 = 0
    @ObservationIgnored private var spotifySpokenPlaybackAccountKey: SpotifyPreferences.AccountKey?
    @ObservationIgnored private var spotifySpokenPlayback: [(id: String, queryID: UInt64, positionMs: UInt64, pausedSeek: Bool)] = []
    @ObservationIgnored private var initialSpotifyProjection = true
    @ObservationIgnored private var spotifyStartDeviceID: String?
    @ObservationIgnored private var spotifyStartTicket: UInt64?
    @ObservationIgnored private var spotifyControlSerial: UInt64 = 0
    @ObservationIgnored private var spotifyStartOperationID: UInt64?
    @ObservationIgnored private var spotifyStartAccepted = false
    @ObservationIgnored private var spotifyStartLocalBaseline: (observation: UInt64, generation: UInt64, wasPlayingTarget: Bool)?
    @ObservationIgnored private var spotifyStartUncertain = false
    private var spotifyStartSentAt: Double?
    private var spotifyStartRestoresSession = false
    private var spotifyStartPauseRequested = false
    @ObservationIgnored private var restoredResumeCancellationRequests: [UInt64: UInt64] = [:]
    private struct TimedOutSpotifyPause {
        let operation: UInt64
        let device: String
        let bridge: UInt64
        let runtime: UInt64
        let account: SpotifyPreferences.AccountKey?
        let restoresSession: Bool
        let expires: ContinuousClock.Instant
    }
    @ObservationIgnored private var timedOutSpotifyPause: TimedOutSpotifyPause?
    @ObservationIgnored private var submittedSpotifyStartScope: (bridge: UInt64, operation: UInt64, runtime: UInt64, processID: Int32?, credentials: UInt64, account: SpotifyPreferences.AccountKey?)?
    @ObservationIgnored private var spotifyStartCredentialEpoch: UInt64?
    @ObservationIgnored private var spotifyStartAccount: SpotifyPreferences.AccountKey?
    @ObservationIgnored private var spotifyStartDeadline: ContinuousClock.Instant?
    private struct UnclaimedSpotifyActivation {
        let bridge, runtime, operation: UInt64
        let processID: Int32
    }
    // Activation makes an owned endpoint discoverable; it is not Play. Keep
    // this denial after cancellation until an explicit target really plays.
    @ObservationIgnored private var unclaimedSpotifyActivation: UnclaimedSpotifyActivation?
    @ObservationIgnored private var restoredPauseOutput: (request: UInt64, device: String, media: String, runtime: UInt64, expires: Double)?
    @ObservationIgnored private var spotifyStartPosition: UInt64 = 0
    @ObservationIgnored private var spotifyStartContextObservation: UInt64 = 0
    @ObservationIgnored private var forwardedOutputClock: (generation: UInt64, sequence: UInt64, device: String, state: PlaybackState, active: Bool)?
    private struct AcceptedOutputScope {
        let bridge, generation, sequence: UInt64
        let processID: Int32
        let device: String
        let uptime: Double
    }
    @ObservationIgnored private var pendingOutputScopes: [UInt64: AcceptedOutputScope] = [:]
    @ObservationIgnored private var acceptedOutputScope: AcceptedOutputScope?
    @ObservationIgnored private var forwardedOutputQueue: (generation: UInt64, sequence: UInt64, device: String)?
    @ObservationIgnored private var pendingSpotifyStart: Task<Void, Never>?
    private(set) var spotifyStartingContextURI: String?
    @ObservationIgnored private var timedOutSpotifyContextURI: String?
    private var spotifyStartingItem: MediaItem?
    var spotifyStartingItemID: String? { spotifyStartingItem?.id }
    @ObservationIgnored private var timedOutSpotifyItemID: String?
    @ObservationIgnored private let remoteConfirmationTimeout: Duration
    @ObservationIgnored private let remotePauseConfirmationGrace: Duration
    private struct SpotifyNextProjection {
        static let limit = 8
        let device: String
        var remaining = 1
        var prefix: [MediaItem] = []
        var queueAvailable = false
        var prefixIsAmbiguous = false

        var preview: MediaItem? { queueAvailable && prefix.indices.contains(remaining - 1) ? prefix[remaining - 1] : nil }

        mutating func observeQueue(_ queue: QueueState) {
            queueAvailable = queue.deviceQueue != nil
            guard queueAvailable, !prefixIsAmbiguous, let current = queue.current else { return }
            var seen = Set([current.id])
            let rows = Array(queue.upcoming.prefix(Self.limit).prefix { seen.insert($0.id).inserted })
            let common = zip(prefix, rows).prefix { $0.id == $1.id }.count
            if common < min(prefix.count, rows.count) {
                // A shifted queue cannot establish which occurrence the earlier clicks targeted.
                prefix = Array(rows.prefix(common))
                prefixIsAmbiguous = true
            } else {
                prefix = rows
            }
            if rows.count < min(queue.upcoming.count, Self.limit) {
                prefix = Array(prefix.prefix(rows.count))
                prefixIsAmbiguous = true
            }
        }
    }
    private struct PendingSkip {
        struct SpotifyReturn {
            let previousRequest: UInt64
            let item: MediaItem
            let predecessorID: String
            let contextURI: String
            let outgoingEpoch: UInt64
            var predecessorEpoch: UInt64?
        }
        let request: UInt64
        let provider: String?
        var currentID: String?
        var positionMs: UInt64
        let previousPreview: MediaItem?
        var spotifyNext: SpotifyNextProjection?
        var localPlayback: (play: Bool, request: UInt64?)?
        let ownedNext: (device: String, generation: UInt64)?
        let ownedSeekBeforePrevious: PendingOwnedSeek?
        var ownedPrevious: (device: String, generation: UInt64)?
        var spotifyReturn: SpotifyReturn?
        var preview: MediaItem? { spotifyReturn?.item ?? spotifyNext?.preview ?? previousPreview }
    }
    private var pendingSkip: PendingSkip?
    private var pendingOwnedPlayback: (request: UInt64, play: Bool, deadline: Double, sawOpposite: Bool)?
    private(set) var trackTransitionDirection = 0
    nonisolated static let previousRestartMilliseconds: UInt64 = 4_000
    @ObservationIgnored private var pendingSkipTimeout: Task<Void, Never>?
    @ObservationIgnored private var nextArtworkTask: Task<Void, Never>?
    @ObservationIgnored private var nextArtworkTarget: (item: String, reference: String)?
    @ObservationIgnored private var ownedOutputSession: Int32?
    private struct OwnedDeviceContinuation {
        let device: RemoteDevice
        let processID: Int32
        let runtime, bridge: UInt64
        var observedUptime: Double
    }
    @ObservationIgnored private var ownedDeviceContinuation: OwnedDeviceContinuation?
    private(set) var reactiveLighting = UserDefaults.standard.object(forKey: "AuvraReactiveLighting") as? Bool ?? true
    private static let spotifySpatialStereoPreference = "AuvraSpotifySpatialStereo"
    private(set) var spotifySpatialStereo: Bool
    @ObservationIgnored private var analysisVisible = false
    @ObservationIgnored private var hostAnalysisLease: HostAnalysisLease?
    @ObservationIgnored private var hostAnalysisGeneration: UInt64 = 0
    @ObservationIgnored private var hostAnalysisClock: SoloistClock?
    @ObservationIgnored private var hostAnalysisRuntime: UInt64 = 0
    @ObservationIgnored private var hostAnalysisSubmittedSequence: UInt64 = 0
    @ObservationIgnored private var hostAnalysisFence: UInt64 = 0
    @ObservationIgnored private var hostAnalysisFenceRuntime: UInt64 = 0
    @ObservationIgnored private let spatialIntentTime: SpatialIntentTimeSource?
    @ObservationIgnored private var hostAnalysisNotBefore: Double = 0
    @ObservationIgnored private var hostRelayPause: (request: UInt64, deadline: Double)?
    @ObservationIgnored private var spatialNextOrigin: (mediaId: String, deadline: Double)?

    private static let playbackSetupIssue = "Set up Auvra playback in Settings → Account. Pair the Auvra device once, then Spotify can stay closed."
    private static let playbackTimeoutIssue = "Playback has not been confirmed yet. Check the selected device and try again."
    private static let spotifyTransferInProgressIssue = "Wait for Spotify to switch playback devices."
    @ObservationIgnored private let libraryFiles: LibraryFileAccess
    @ObservationIgnored private let sessionFiles: SessionFileBookmarks
    @ObservationIgnored private let sessionEnabled: Bool
    @ObservationIgnored private var libraryQueryId: UInt64 = 0
    @ObservationIgnored private let fileAccess = LocalFileAccess()
    @ObservationIgnored private var bridge: (any PlayerConnection)?
    @ObservationIgnored private let connectionFactory: PlayerConnectionFactory
    @ObservationIgnored private var systemMedia: (any SystemMediaSession)?
    @ObservationIgnored private var nextRequestId: UInt64 = 1
    @ObservationIgnored private var positionAnchorTime = ProcessInfo.processInfo.systemUptime
    @ObservationIgnored private var visualReceiver: (@MainActor (VisualModulation?) -> Void)?
    @ObservationIgnored private var lastVisualModulation: VisualModulation?
    @ObservationIgnored private var analysisReceivedAt: TimeInterval = 0
    @ObservationIgnored private var artworkTask: Task<Void, Never>?
    @ObservationIgnored private var artworkReference: String?
    @ObservationIgnored private var artworkReceiver: (@MainActor (ArtworkPalette?) -> Void)?
    private(set) var artworkPalette: ArtworkPalette?

    init(libraryDirectory: URL? = nil, loadLibrary: Bool = true, loadSession: Bool = true, loadSpotify: Bool = false,
         connectionFactory: PlayerConnectionFactory? = nil, playbackRuntime: SoloistRuntime? = nil,
         remoteConfirmationTimeout: Duration = .seconds(15), remotePauseConfirmationGrace: Duration = .seconds(5), lyricsProvider: (any LyricsProviding)? = nil,
         spotifyPreferences: SpotifyPreferences? = nil, spotifyClientID: String? = nil,
         spotifyAuthorizationFactory: SpotifyAuthorizationFactory? = nil, volumeDefaults: UserDefaults = .standard,
         spatialIntentTime: SpatialIntentTimeSource? = nil) {
        self.spatialIntentTime = spatialIntentTime
        self.spotifyPreferences = spotifyPreferences ?? SpotifyPreferences()
        self.volumeDefaults = volumeDefaults
        let spatialEnabled = volumeDefaults.bool(forKey: Self.spotifySpatialStereoPreference)
        spotifySpatialStereo = spatialEnabled
        if loadSpotify, spatialEnabled { SpatialOutputPreflight.start() }
        self.spotifyClientID = spotifyClientID ?? UserDefaults.standard.string(forKey: "AuvraSpotifyClientID") ?? ""
        self.spotifyAuthorizationFactory = spotifyAuthorizationFactory ?? { SpotifyAuthorization(status: $0, access: $1, disconnected: $2) }
        lyrics = LyricsStore(provider: lyricsProvider ?? LyricsService(directory: libraryDirectory?.appendingPathComponent("Lyrics", isDirectory: true)))
        loadsLibrary = loadLibrary; loadsSpotify = loadSpotify
        self.connectionFactory = connectionFactory ?? { PlayerBridge(receive: $0, failure: $1) }
        spotifyPlayback = playbackRuntime ?? SoloistRuntime()
        self.remoteConfirmationTimeout = remoteConfirmationTimeout
        self.remotePauseConfirmationGrace = remotePauseConfirmationGrace
        libraryFiles = LibraryFileAccess(directory: libraryDirectory)
        sessionFiles = SessionFileBookmarks(directory: libraryDirectory)
        sessionEnabled = loadLibrary && loadSession
        connectBridge()
        spotifyPlayback.changed = { [weak self] in
            guard let self else { return }; forwardOutputClock(); publishSystemMedia()
            if loadsSpotify { refreshSpotify() }
        }
        spotifyPlayback.observed = { [weak self] in self?.forwardOutputClock(); self?.publishSystemMedia() }
        spotifyPlayback.interrupted = { [weak self] in self?.ownedOutputInterrupted($0) }
        spotifyPlayback.resetting = { [weak self] in self?.cancelOwnedOutputRecovery() }
        if loadLibrary { initialRestoreTask = Task { [weak self] in await self?.restoreLibrary() } }
        if loadSpotify { prepareSpotify() }
    }
    private func connectBridge() {
        restoredResumeCancellationRequests.removeAll()
        pendingOutputScopes.removeAll(); acceptedOutputScope = nil
        bridgeEpoch &+= 1; let epoch = bridgeEpoch
        bridge = connectionFactory({ [weak self] event in
            guard let self, self.bridgeEpoch == epoch else { return }; self.apply(event)
        }, { [weak self] requestId, message in
            guard let self, self.bridgeEpoch == epoch else { return }
            if let requestId {
                pendingOutputScopes.removeValue(forKey: requestId)
                if pendingOutputUpdates.remove(requestId) == nil { pendingCommands.remove(requestId) }
                if restoredResumeCancellationRequests.removeValue(forKey: requestId) != nil { return }
            } else {
                pendingOutputScopes.removeAll(); acceptedOutputScope = nil
                pendingOutputUpdates.removeAll()
                pendingCommands.removeAll()
                restoredResumeCancellationRequests.removeAll()
                bridgeFailed = true
            }
            if requestId == localLoadingSeekIntent?.request || requestId == nil { localLoadingSeekIntent = nil }
            if requestId == pendingOwnedSeek?.request || requestId == nil { pendingOwnedSeek = nil }
            if requestId == pendingLocalPlayback?.request || requestId == nil { pendingLocalPlayback = nil }
            if requestId == restoredPauseOutput?.request || requestId == nil { restoredPauseOutput = nil }
            rejectHostRelayPause(requestId)
            if requestId == spotifyStartTicket || requestId == nil { cancelSpotifyStart() }
            if requestId == pendingSpotifyTransfer?.request || requestId == nil {
                clearSpotifyTransfer()
                if requestId != nil { forwardOutputClock(); publishSystemMedia() }
            }
            if requestId == pendingSkip?.request || requestId == pendingSkip?.localPlayback?.request || requestId == nil { clearPendingSkip() }
            if requestId == pendingVolume?.request || requestId == nil { clearPendingVolume() }
            clearRejectedQueueModes(requestId)
            if requestId == spotifyEditTicket || requestId == nil { spotifyEditPending = false; spotifyEditTicket = nil; spotifyEditIssue = message }
            if requestId == soundRequest || requestId == nil { soundDraft = nil; soundRequest = nil }
            if let requestId, let queryID = spotifyMainQueryTickets.removeValue(forKey: requestId) {
                failSpotifyMainQuery(queryID, issue: PlayerIssue(code: "unavailable", message: message)); return
            }
            if requestId == nil {
                spotifyMainQueryTickets.removeAll()
                if isLoadingSpotifyPage { failSpotifyMainQuery(spotifyQuery.queryId, issue: PlayerIssue(code: "unavailable", message: message)) }
            }
            if let requestId, let queryID = spotifyDetailTickets[requestId] { failSpotifyDetail(queryID, message: message); return }
            else if requestId == nil { for queryID in Array(spotifyDetailQueries.keys) { failSpotifyDetail(queryID, message: message) } }
            analysisObservation = nil; publishVisualModulation(); errorMessage = message
        })
    }
    func restartPlayer() async {
        guard !isRestarting, !isClosing else { return }
        isRestarting = true; defer { isRestarting = false }
        localLoadingSeekIntent = nil; pendingOwnedSeek = nil; pendingLocalPlayback = nil
        cancelSpotifyStart(); clearSpotifyTransfer(); clearPendingSkip(); clearPendingVolume(); clearRejectedQueueModes(nil)
        spotifyEditIssue = spotifyEditPending ? "The player restarted before Spotify confirmed the change. Check your library before retrying." : nil
        spotifyEditPending = false; spotifyEditTicket = nil; spotifyOperationID &+= 1; spotifyEditSuccess = nil; spotifyCreatedPlaylist = nil
        initialSpotifyProjection = snapshot?.activeProvider == "spotify"
        clearHostAnalysis(intent: true)
        spotifyMainQueryTickets.removeAll()
        bridgeEpoch &+= 1; ownedOutputSession = nil; analysisObservation = nil; publishVisualModulation(); systemMedia?.clear()
        forwardedOutputClock = nil
        forwardedOutputQueue = nil
        initialRestoreTask?.cancel(); await initialRestoreTask?.value; initialRestoreTask = nil
        spotifyAuth?.shutdown(); spotifyAuth = nil
        // Restart restores the queue on pause. The owned runtime outlives the
        // Rust bridge, so stop that exact child before discarding its controls;
        // otherwise it can keep playing directly behind a paused restored UI.
        // External Connect playback receives no pause or stop command here.
        await spotifyPlayback.shutdown()
        if let old = bridge { await withCheckedContinuation { continuation in old.close { continuation.resume() } } }
        guard !isClosing else { return }
        bridge = nil; snapshot = nil; lyrics.select(nil); pendingCommands.removeAll(); pendingOutputUpdates.removeAll(); tracks.removeAll()
        clearSpotifyReadState(); spotifySaved.removeAll(); remoteStates.removeAll(); spotifyState = nil; spotifyPage = nil
        libraryState = nil; libraryPage = nil; soundDraft = nil; soundRequest = nil
        errorMessage = nil; bridgeFailed = false; connectBridge()
        if loadsLibrary { await restoreLibrary() }
        if loadsSpotify { prepareSpotify() }
    }
    var isSavingSound: Bool { soundDraft != nil || snapshot?.sound?.saving == true }
    var soundSettings: SoundSettings { soundDraft ?? snapshot?.sound?.settings ?? SoundSettings() }
    func updateSound(_ change: (inout SoundSettings) -> Void) {
        var value = soundSettings; change(&value)
        soundDraft = value; soundRequest = send(.setSound(value))
    }
    var canRetryPlayback: Bool { snapshot?.state == .error && snapshot?.transport.retry == true && canPlay && !bridgeFailed }
    func retryPlayback() { guard canRetryPlayback else { return }; errorMessage = nil; send(.retryPlayback) }
    var audioAnalysis: AudioAnalysisFrame? {
        guard let snapshot, snapshot.state == .playing, snapshot.capabilities.pcmAnalysis,
              let current = snapshot.queue.current, analysisObservation?.mediaId == current.id,
              let frame = analysisObservation?.frame, frame.spectrum.count == 32,
              frame.spectrum.allSatisfy({ $0.isFinite && (0...1).contains($0) }) else { return nil }
        if current.capabilities.pcmAnalysis {
            guard snapshot.providerConnection == .connected else { return nil }
        } else {
            guard let lease = hostAnalysisLease, lease.visuals, lease.playing, frame.generation == lease.generation,
                  lease.processId == spotifyPlayback.status.processID, hostAnalysisRuntime == spotifyPlayback.clockGeneration,
                  lease.mediaId == current.id,
                  ownedAnalysisDevice(for: snapshot, at: ProcessInfo.processInfo.systemUptime)?.id == lease.deviceId else { return nil }
        }
        return frame
    }
    var current: MediaItem? { snapshot?.queue.current }
    /// The user's selection may be displayed before the audio output confirms it.
    /// `current`, the playback clock and system ownership remain authoritative.
    var displayedTrack: MediaItem? { spotifyStartingItem ?? pendingSkip?.preview ?? current }
    var isChangingTrack: Bool { spotifyStartingItem != nil || pendingSkip != nil }
    var displayedPosition: Double {
        if spotifyStartingItem != nil { return Double(spotifyStartPosition) / 1000 }
        if pendingSkip != nil { return 0 }
        return position()
    }
    var visualModulation: VisualModulation? {
        guard reactiveLighting, let frame = audioAnalysis, let modulation = analysisObservation?.modulation,
              ProcessInfo.processInfo.systemUptime - analysisReceivedAt <= 0.3,
              modulation.generation == frame.generation, modulation.sampleRate == frame.sampleRate,
              modulation.windowEndFrame == frame.windowEndFrame, modulation.controls.isValid,
              abs(modulation.controls.timeSeconds - Double(frame.windowEndFrame) / Double(frame.sampleRate)) < 0.000_001 else { return nil }
        return modulation
    }
    func attachVisualModulation(_ receive: @escaping @MainActor (VisualModulation?) -> Void) {
        visualReceiver = receive; lastVisualModulation = visualModulation; receive(lastVisualModulation)
    }
    func attachArtworkPalette(_ receive: @escaping @MainActor (ArtworkPalette?) -> Void) {
        artworkReceiver = receive; receive(artworkPalette); updateArtworkPalette()
    }
    private func updateArtworkPalette() {
        let reference = displayedTrack?.artwork
        guard reference != artworkReference else { return }
        artworkTask?.cancel(); artworkReference = reference
        // Keep the previous cover's ambient colors until its replacement is
        // decoded. An artwork fetch must not briefly expose a default preset.
        guard let reference else { artworkPalette = nil; artworkReceiver?(nil); return }
        artworkTask = Task { [weak self] in
            let palette = await LocalArtworkLoader.shared.palette(for: reference)
            guard let self, !Task.isCancelled, artworkReference == reference else { return }
            artworkPalette = palette; artworkReceiver?(palette)
        }
    }
    private func prewarmNextArtwork() {
        let target: (item: String, reference: String)? = {
            guard !isClosing, !bridgeFailed, !spotifyReadRevoked, let snapshot, snapshot.activeProvider == "spotify", snapshot.state == .playing, !isChangingTrack,
                  snapshot.queue.deviceQueue != nil, let item = snapshot.queue.upcoming.first,
                  let reference = item.artwork, reference != displayedTrack?.artwork else { return nil }
            return (item.id, reference)
        }()
        guard target?.item != nextArtworkTarget?.item || target?.reference != nextArtworkTarget?.reference else { return }
        nextArtworkTask?.cancel(); nextArtworkTask = nil; nextArtworkTarget = target
        guard let target else { return }
        nextArtworkTask = Task(priority: .utility) {
            // Let transport and the current cover take priority after each switch.
            do { try await Task.sleep(for: .milliseconds(350)) } catch { return }
            await LocalArtworkLoader.shared.prewarm(target.reference)
        }
    }
    private func publishVisualModulation() {
        let value = visualModulation
        guard value != lastVisualModulation else { return }
        lastVisualModulation = value; visualReceiver?(value)
    }
    var isLoading: Bool { snapshot?.state == .loading || isChangingTrack || spotifyStartingContextURI != nil }
    var isLocalPlayback: Bool { current?.capabilities.pcmAnalysis == true }
    var demoTracks: [MediaItem] { tracks.filter { !$0.capabilities.pcmAnalysis } }
    var localTracks: [MediaItem] { tracks.filter { $0.capabilities.pcmAnalysis } }
    var isPlaying: Bool { snapshot?.state == .playing }
    var playbackIssue: String? {
        guard let id = snapshot?.activeProvider else { return nil }
        return remoteStates[id]?.issue?.message
    }
    var canPlay: Bool { !isRestarting && !isClosing && !bridgeFailed && current != nil && (snapshot?.capabilities.play == true || canBootstrapRestoredSpotifySelection) }
    var canNavigateQueue: Bool { !isRestarting && !isClosing && !bridgeFailed && (current != nil || snapshot?.queue.upcoming.isEmpty == false) }
    var canGoNext: Bool { canNavigateQueue && snapshot?.transport.next == true }
    var canGoPrevious: Bool {
        if hasPendingLocalSeek { return canSeek }
        if previousRestartsTrack { return canSeek }
        return canGoToPreviousTrack
    }
    private var previousRestartsTrack: Bool {
        let position = pendingOwnedSeek.flatMap { matchingOwnedSeekClock($0) == nil ? nil : Double($0.positionMs) }
            ?? positionMilliseconds()
        return !ownedRelativeHandoff && position >= Double(Self.previousRestartMilliseconds)
    }
    private var canGoToPreviousTrack: Bool {
        canNavigateQueue && (snapshot?.transport.previous == true || hasPendingOwnedNext)
    }
    var canStop: Bool { canNavigateQueue && snapshot?.transport.stop == true }
    var canEditQueue: Bool { !isRestarting && !isClosing && !bridgeFailed && snapshot?.transport.queueEdit == true }
    var canShuffle: Bool { canNavigateQueue && snapshot?.transport.shuffle == true }
    var canRepeat: Bool { canNavigateQueue && snapshot?.transport.repeat == true }
    var shuffleEnabled: Bool { pendingShuffle?.value ?? snapshot?.queue.shuffle ?? false }
    var repeatMode: RepeatMode? { pendingRepeat?.value ?? snapshot?.queue.repeatMode }
    var sourceLabel: String { current == nil ? "No active track" : (current?.location?.service ?? (isLocalPlayback ? "Local file" : "Silent demo")) }
    func queueIssue(_ id: String) -> String? { snapshot?.queueIssues.first(where: { $0.id == id })?.error.message }
    private var ownsPlaybackControl: Bool {
        guard snapshot?.activeProvider == "spotify", snapshot?.restoredRemote == nil, !spotifyReadRevoked,
              spotifyState?.ownedOutputControl == true, spotifyState?.authenticationRequired == false,
              let device = ownedSpotifyDevice, let active = spotifyState?.playback?.device,
              active.id == device.id, active.isActive, !active.isRestricted else { return false }
        return spotifyOutputSelectionAllows(device.id)
    }
    private var ownedRelativeHandoff: Bool {
        ownsPlaybackControl && spotifyStartingItemID == nil && spotifyStartingContextURI == nil
            && (pendingSkip?.provider == "spotify" || snapshot?.state == .loading)
    }
    private var hasPendingOwnedNext: Bool {
        guard ownedRelativeHandoff, let next = pendingSkip?.ownedNext else { return false }
        return next.device == ownedSpotifyDevice?.id && next.generation == spotifyPlayback.clockGeneration
    }
    private var hasPendingOwnedPlayback: Bool {
        ownsPlaybackControl && pendingOwnedPlayback.map { ProcessInfo.processInfo.systemUptime < $0.deadline } == true
    }
    private var hasLocalPCMOutput: Bool {
        isLocalPlayback && snapshot?.capabilities.pcmAnalysis == true && snapshot?.providerConnection == .connected
    }
    private var localSeekAppliesToCurrentTrack: Bool {
        guard hasLocalPCMOutput, let intent = localLoadingSeekIntent else { return false }
        return intent.provider == snapshot?.activeProvider && intent.mediaID == current?.id
    }
    private var hasPendingLocalSeek: Bool {
        localSeekAppliesToCurrentTrack && localLoadingSeekIntent?.resolvesPreviousInEngine == true
    }
    private var ownedSeekClock: SoloistClock? {
        guard ownsPlaybackControl, !isClosing, !bridgeFailed, !hasPendingSpotifySelection,
              spotifyPlayback.status.controlSupported, spotifyPlayback.status.authenticated == true,
              let clock = spotifyPlayback.clock, clock.active,
              clock.isValid(at: ProcessInfo.processInfo.systemUptime),
              SpotifyPlaybackIdentity.matches(current?.id, clock.mediaId) else { return nil }
        return clock
    }
    private func matchingOwnedSeekClock(_ seek: PendingOwnedSeek) -> SoloistClock? {
        guard ProcessInfo.processInfo.systemUptime < seek.deadline, let clock = ownedSeekClock,
              clock.mediaId == seek.mediaID, clock.queueEpoch == seek.queueEpoch,
              clock.sequence >= seek.sequence, ownedSpotifyDevice?.id == seek.deviceID,
              spotifyPlayback.clockGeneration == seek.generation else { return nil }
        return clock
    }
    private func reconcileOwnedSeek() {
        guard let seek = pendingOwnedSeek else { return }
        guard let clock = matchingOwnedSeekClock(seek) else { pendingOwnedSeek = nil; return }
        // Admission alone does not apply the seek. Wait until both clocks have
        // crossed to the requested side of the Previous boundary.
        let threshold = Self.previousRestartMilliseconds
        let observed = positionMilliseconds()
        guard [.playing, .paused].contains(clock.state), clock.sequence > seek.sequence,
              (clock.positionMs >= threshold) == (seek.positionMs >= threshold),
              (observed >= Double(threshold)) == (seek.positionMs >= threshold),
              abs(Double(clock.positionMs) - Double(seek.positionMs)) <= 250,
              abs(observed - Double(clock.positionMs)) <= 250 else { return }
        pendingOwnedSeek = nil
    }
    private var localHandoffPlayIntent: Bool? {
        guard hasLocalPCMOutput, let snapshot, let pendingSkip, pendingSkip.provider == snapshot.activeProvider,
              [.playing, .paused, .loading].contains(snapshot.state) else { return nil }
        return pendingSkip.localPlayback?.play
    }
    private var localPlaybackToggleIntent: Bool? {
        localHandoffPlayIntent ?? pendingLocalPlayback?.play
    }
    private var waitingForRestoredSpotifyDevice: Bool {
        snapshot?.activeProvider == "spotify" && snapshot?.restoredRemote != nil
            && spotifyStartRestoresSession && spotifyStartingItemID != nil && spotifyStartSentAt == nil
    }
    private var hasPendingSpotifySelection: Bool {
        !spotifyReadRevoked && (!spotifyStartRestoresSession || spotifyStartSentAt != nil)
            && (spotifyStartingItem != nil || spotifyStartingContextURI != nil)
    }
    var playbackToggleShowsPause: Bool {
        if waitingForRestoredSpotifyDevice { return true }
        if hasPendingSpotifySelection { return !spotifyStartPauseRequested }
        if hasPendingOwnedPlayback, let pendingOwnedPlayback { return pendingOwnedPlayback.play }
        return localPlaybackToggleIntent ?? (isPlaying || ownedRelativeHandoff)
    }
    var canTogglePlayback: Bool {
        guard !bridgeFailed, !isRestarting, !isClosing else { return false }
        if waitingForRestoredSpotifyDevice || hasPendingSpotifySelection || ownedRelativeHandoff || hasPendingOwnedPlayback { return true }
        if canBootstrapRestoredSpotifySelection { return true }
        if let play = localPlaybackToggleIntent {
            return (play ? snapshot?.capabilities.pause : snapshot?.capabilities.play) == true
        }
        guard !isLoading else { return false }
        if current != nil { return (isPlaying ? snapshot?.capabilities.pause : snapshot?.capabilities.play) == true }
        guard let first = snapshot?.queue.upcoming.first else { return false }
        return first.capabilities.play && queueIssue(first.id) == nil
    }
    private var canBootstrapRestoredSpotifySelection: Bool {
        snapshot?.activeProvider == "spotify" && snapshot?.restoredRemote != nil
            && current?.capabilities.play == true && selectedSpotifyDeviceID == nil
            && !spotifyReadRevoked && spotifyState?.authenticationRequired == false
            && spotifyState?.connection == .connected && spotifyPlayback.canActivateForSelection
    }
    var canSeek: Bool {
        guard !isRestarting, !isClosing, !bridgeFailed, let current, current.durationMs > 0, snapshot?.capabilities.seek == true else { return false }
        if !isLoading { return true }
        return snapshot?.state == .loading && !isChangingTrack
            && spotifyStartingContextURI == nil && localSeekAppliesToCurrentTrack
    }
    var skipForwardSeconds: Double { snapshot?.transport.seekForwardMs.flatMap { (1...300_000).contains($0) ? Double($0) / 1000 : nil } ?? 10 }
    var skipBackwardSeconds: Double { snapshot?.transport.seekBackwardMs.flatMap { (1...300_000).contains($0) ? Double($0) / 1000 : nil } ?? 10 }
    var volume: Double { pendingVolume?.value ?? Double(snapshot?.volume ?? 0.7) }
    // A restored remote selection carries the local session's volume until an
    // actual remote playback snapshot arrives. It cannot describe that output.
    var displayedVolume: Double? {
        guard snapshot?.restoredRemote == nil else { return nil }
        return volume
    }
    var isMuted: Bool { volume == 0 }
    private var volumeDeviceID: String? { snapshot?.activeProvider == "spotify" ? spotifyState?.playback?.device?.id : nil }
    func position() -> Double { positionMilliseconds() / 1000 }
    func positionMilliseconds() -> Double {
        guard let snapshot, let current else { return 0 }
        let clockRuns = snapshot.providerConnection == .connected || snapshot.playbackSessionActive
        let elapsed = isPlaying && clockRuns ? max(0, ProcessInfo.processInfo.systemUptime - positionAnchorTime) : 0
        let rate = snapshot.playbackRate.flatMap { $0.isFinite && (0...4).contains($0) ? $0 : nil } ?? 1
        return min(Double(current.durationMs), Double(snapshot.positionMs) + elapsed * rate * 1000)
    }
    @discardableResult
    func send(_ command: PlayerCommand, outputUpdate: Bool = false) -> UInt64 {
        if isRestarting && Self.isPlaybackCommand(command) {
            let id = nextRequestId; nextRequestId += 1
            let epoch = bridgeEpoch
            let outcome = CommandOutcome(requestId: id, error: PlayerIssue(code: "busy", message: "The player is restarting."))
            Task { @MainActor [weak self] in
                guard let self, !self.isClosing, self.bridgeEpoch == epoch else { return }
                self.apply(.commandResult(outcome))
            }
            return id
        }
        if case .play = command { restoredPauseOutput = nil }
        var relayPauseDeadline: Double?
        var nextOrigin: String?
        var relativeConfirmation = spatialRelativeOrigin(for: command)
        let addressedSpatialPause: Bool
        if case .remote(let provider, .control(.pauseOnDevice(let device))) = command,
           let lease = hostAnalysisLease, lease.spatialStereo, lease.provider == provider, lease.deviceId == device,
           lease.processId == spotifyPlayback.status.processID, hostAnalysisRuntime == spotifyPlayback.clockGeneration {
            addressedSpatialPause = true
        } else { addressedSpatialPause = false }
        let spatialSeekOrigin: SoloistClock? = if case .seek(let position) = command,
            hostAnalysisLease?.spatialStereo == true, hostAnalysisLease?.playing == true,
            let clock = ownedSeekClock, clock.state == .playing, clock.rate > 0,
            clock.queueEpoch.map({ $0 > 0 }) == true, abs(Double(position) - Double(clock.positionMs)) >= 750 {
            clock
        } else { nil }
        if command.invalidatesHostAnalysis {
            spatialNextOrigin = nil
            spatialSeekConfirmation = nil
            spatialPreviousConfirmation = nil
            spatialRelativeConfirmation = nil
            switch command {
            case .play where hostAnalysisLease?.spatialStereo == true || (spotifySpatialStereo && ownsPlaybackControl):
                hostRelayPause = nil
                break
            case .pause where hostAnalysisLease?.spatialStereo == true || (spotifySpatialStereo && ownedRelativeHandoff):
                // A quick Pause during Next can arrive after the old lease has been revoked.
                relayPauseDeadline = ProcessInfo.processInfo.systemUptime + (ownedRelativeHandoff ? 3.5 : 0.8)
                suspendHostRelay()
            case .remote where addressedSpatialPause:
                relayPauseDeadline = ProcessInfo.processInfo.systemUptime + 0.8
                suspendHostRelay()
            case .seek where keepsPausedSpatialRelay:
                // The existing relay is muted and its buffered samples are already empty.
                break
            case .seek where spatialSeekOrigin != nil:
                hostRelayPause = nil
                // The coordinator revokes the capture and can warm the emptied renderer.
                clearHostAnalysis(intent: true, sendNativeRevoke: false)
            case .next where ownsPlaybackControl && hostAnalysisLease?.spatialStereo == true
                && (hostAnalysisLease?.playing == true || keepsPausedSpatialRelay):
                hostRelayPause = nil
                nextOrigin = hostAnalysisLease?.mediaId
                // The coordinator revokes this capture. A confirmed silent renderer can be handed off.
                clearHostAnalysis(intent: true, sendNativeRevoke: false)
                hostAnalysisNotBefore = 0
            default:
                hostRelayPause = nil
                clearHostAnalysis(intent: true)
            }
        }
        let id = nextRequestId; nextRequestId += 1
        trackOwnedPreviousIntent(command, request: id)
        if relativeConfirmation != nil {
            relativeConfirmation?.request = id
            spatialRelativeConfirmation = relativeConfirmation
            traceSpatialRelative("input", reason: relativeConfirmation?.continued == true ? "continued" : "seeded", proof: relativeConfirmation)
        }
        switch command {
        case .setShuffle, .setRepeat: spatialPreviousConfirmation = nil
        default: break
        }
        switch command {
        case .seek, .previous, .skipPrevious, .next, .stop, .openFile, .replaceQueue,
             .playLibrary, .jumpQueue, .clearQueue, .retryPlayback:
            latestPositionRequest = id
            presentationSeek = nil
            if case .seek(let position) = command, let current {
                presentationSeek = (snapshot?.activeProvider, current.id, position, ProcessInfo.processInfo.systemUptime + 2)
            }
        default: break
        }
        switch command {
        case .seek(let position):
            if let clock = ownedSeekClock, let mediaID = clock.mediaId, let device = ownedSpotifyDevice {
                let seek = PendingOwnedSeek(request: id, positionMs: position, generation: spotifyPlayback.clockGeneration,
                                            sequence: clock.sequence, mediaID: mediaID, deviceID: device.id,
                                            queueEpoch: clock.queueEpoch, deadline: ProcessInfo.processInfo.systemUptime + 2)
                pendingOwnedSeek = seek
                if let origin = spatialSeekOrigin, let processID = spotifyPlayback.status.processID {
                    spatialSeekConfirmation = .init(seek: seek, original: origin, processID: processID,
                        contextURI: clock.metadata?.contextUri ?? spotifyState?.playback?.contextUri)
                }
            } else { pendingOwnedSeek = nil }
        case .remote(_, .credential):
            pendingOwnedSeek = nil
        default: break
        }
        switch command {
        case .seek where hasLocalPCMOutput:
            if let provider = snapshot?.activeProvider, let mediaID = current?.id {
                localLoadingSeekIntent = LocalSeekIntent(request: id, provider: provider, mediaID: mediaID,
                                                        resolvesPreviousInEngine: true)
            }
        case .previous where hasPendingLocalSeek:
            // Further presses need the engine's position after this command too.
            localLoadingSeekIntent?.request = id
        case .play where hasLocalPCMOutput && !isLoading && !localSeekAppliesToCurrentTrack && snapshot?.playbackSessionActive == false
            && (snapshot?.state == .paused || snapshot?.state == .stopped):
            // The decoder can accept a seek while it opens this already selected file.
            if let provider = snapshot?.activeProvider, let mediaID = current?.id {
                localLoadingSeekIntent = LocalSeekIntent(request: id, provider: provider, mediaID: mediaID,
                                                        resolvesPreviousInEngine: false)
            }
        case .openFile, .replaceQueue, .playLibrary, .jumpQueue, .clearQueue, .removeQueueItem,
             .stop, .retryPlayback, .next, .previous, .skipPrevious,
             .remote(_, .activate), .remote(_, .adoptOutput), .remote(_, .disconnect), .remote(_, .control):
            localLoadingSeekIntent = nil
        default: break
        }
        switch command {
        case .play, .pause:
            initialSpotifyProjection = false; cancelSpotifyStart()
            if localHandoffPlayIntent != nil {
                if case .play = command { pendingSkip?.localPlayback = (true, id) }
                else { pendingSkip?.localPlayback = (false, id) }
            } else if !ownedRelativeHandoff && pendingSkip?.spotifyReturn == nil {
                let sameLocalItem = pendingSkip == nil && hasLocalPCMOutput && (!isLoading || pendingLocalPlayback != nil)
                if sameLocalItem, let provider = snapshot?.activeProvider, let mediaID = current?.id {
                    let play = if case .play = command { true } else { false }
                    pendingLocalPlayback = PendingLocalPlayback(request: id, provider: provider, mediaID: mediaID, play: play)
                }
                clearPendingSkip()
            }
        case .openFile, .replaceQueue, .playLibrary, .stop, .next, .previous, .skipPrevious, .seek:
            initialSpotifyProjection = false; cancelSpotifyStart(); clearPendingSkip(); pendingOwnedPlayback = nil
        default: break
        }
        switch command {
        case .openFile, .replaceQueue, .playLibrary, .jumpQueue, .clearQueue,
             .stop, .retryPlayback, .next, .previous, .skipPrevious,
             .remote(_, .activate), .remote(_, .adoptOutput), .remote(_, .disconnect),
             .remote(_, .control(.start)), .remote(_, .control(.startOnDevice)),
             .remote(_, .control(.startContext)), .remote(_, .control(.startContextItem)),
             .remote(_, .control(.startSpokenOnDevice)), .remote(_, .control(.resumeOnDevice)), .remote(_, .control(.transfer)):
            pendingLocalPlayback = nil; pendingOwnedSeek = nil
        default: break
        }
        if let relayPauseDeadline { hostRelayPause = (id, relayPauseDeadline) }
        if let nextOrigin { spatialNextOrigin = (nextOrigin, ProcessInfo.processInfo.systemUptime + 1.25) }
        trackQueueMode(command, request: id)
        if outputUpdate {
            pendingOutputUpdates.insert(id)
        } else {
            pendingCommands.insert(id)
        }
        if case .remote(_, .query(let query)) = command, !query.kind.isDetail { spotifyMainQueryTickets[id] = query.queryId }
        bridge?.send(command, requestId: id)
        return id
    }
    private static func isPlaybackCommand(_ command: PlayerCommand) -> Bool {
        switch command {
        case .play, .pause, .stop, .next, .previous, .skipPrevious, .seek, .openFile, .replaceQueue,
             .playLibrary, .jumpQueue, .clearQueue, .playNext, .removeQueueItem, .retryPlayback,
             .setVolume, .setShuffle, .setRepeat, .pauseConfirmedRestoredResume: true
        case .remote(_, .control): true
        default: false
        }
    }
    private func trackOwnedPreviousIntent(_ command: PlayerCommand, request: UInt64) {
        switch command {
        case .previous, .skipPrevious:
            guard snapshot?.activeProvider == "spotify", let device = ownedSpotifyDevice,
                  let account = spotifyAccountIdentity?.key, let processID = spotifyPlayback.status.processID else {
                ownedPreviousIntent = nil
                return
            }
            ownedPreviousIntent = .init(request: request, bridge: bridgeEpoch, credentials: spotifyCredentialEpoch,
                runtime: spotifyPlayback.clockGeneration, processID: processID, deviceID: device.id, account: account,
                deadline: ContinuousClock.now.advanced(by: .seconds(3)))
        case .playNext, .removeQueueItem, .retryPlayback, .setShuffle, .setRepeat,
             .remote(_, .adoptOutput), .ownedOutputInterrupted, .cancelOwnedOutputRecovery:
            ownedPreviousIntent = nil
        default:
            if command.invalidatesHostAnalysis { ownedPreviousIntent = nil }
        }
    }
    private func allowsOwnedPreviousRecovery(_ command: RemoteOutputCommand) -> Bool {
        guard let intent = ownedPreviousIntent, ContinuousClock.now < intent.deadline,
              command.requestId == intent.request, command.deviceId == intent.deviceID,
              command.generation == intent.runtime, spotifyPlayback.clockGeneration == intent.runtime,
              spotifyPlayback.status.processID == intent.processID, spotifyPlayback.status.authenticated == true,
              bridgeEpoch == intent.bridge, spotifyCredentialEpoch == intent.credentials,
              spotifyAccountIdentity?.key == intent.account, snapshot?.activeProvider == "spotify",
              !isClosing, !isRestarting, !bridgeFailed, !spotifyReadRevoked,
              !hasPendingSpotifySelection, pendingSpotifyTransfer == nil,
              ownedSpotifyDevice?.id == intent.deviceID, spotifyOutputSelectionAllows(intent.deviceID),
              spotifyState?.ownedOutputControl == true || spotifyState?.ownedOutputObserved == true else { return false }
        return true
    }
    func play(_ item: MediaItem) {
        let collection = item.capabilities.pcmAnalysis ? localTracks : demoTracks
        guard let index = collection.firstIndex(where: { $0.id == item.id }) else { return }
        send(.replaceQueue(items: collection, start: index, context: .userQueue))
    }
    func chooseFile() async {
        guard !isChoosingFile else { return }
        isChoosingFile = true; defer { isChoosingFile = false }
        if let url = await fileAccess.chooseAudioFile() { openFile(url) }
    }
    func openFile(_ url: URL) {
        guard url.isFileURL else { errorMessage = "Choose a local audio file."; return }
        ownedPreviousIntent = nil
        fileAccess.retainAccess(to: url)
        let epoch = bridgeEpoch
        if sessionEnabled {
            Task { [weak self] in
                guard let self else { return }
                do {
                    try await sessionFiles.remember(url)
                    guard epoch == bridgeEpoch else { return }
                    send(.openFile(path: url.resolvingSymlinksInPath().standardizedFileURL.path))
                    await refreshQueueAccess()
                } catch { guard epoch == bridgeEpoch else { return }; errorMessage = "File access could not be saved: \(error.localizedDescription)" }
            }
        } else { send(.openFile(path: url.path)) }
    }
    func togglePlayback() {
        let admitted = canTogglePlayback
        if Self.tracePlayback, snapshot?.activeProvider == "spotify" {
            Self.playbackTiming.info("output_toggle admitted=\(admitted, privacy: .public) next_pending=\(self.pendingSkip != nil, privacy: .public) wants_play=\(!self.playbackToggleShowsPause, privacy: .public) owned=\(self.ownsPlaybackControl, privacy: .public)")
        }
        guard admitted else { return }
        if waitingForRestoredSpotifyDevice {
            cancelSpotifyStart()
            return
        }
        if hasPendingSpotifySelection {
            spotifyStartPauseRequested.toggle()
            if spotifyStartRestoresSession, spotifyStartPauseRequested,
               let operation = spotifyStartOperationID, let device = spotifyStartDeviceID,
               !restoredResumeCancellationRequests.values.contains(operation) {
                let request = send(.cancelRestoredResume(operationId: operation, deviceId: device),
                                   outputUpdate: true)
                restoredResumeCancellationRequests[request] = operation
            }
            return
        }
        if let resume = snapshot?.restoredRemote, let item = current, snapshot?.activeProvider == "spotify" {
            startSpotifySelection(item: item, collection: nil, resume: resume, positionMs: snapshot?.positionMs ?? 0)
        } else if unclaimedSpotifyActivation != nil, !isPlaying, selectedSpotifyDeviceID == nil,
                  snapshot?.activeProvider == "spotify" {
            guard let item = current, item.capabilities.play,
                  let reference = SpotifyContentReference(item.id),
                  spotifyState?.playback?.current?.id == item.id else {
                errorMessage = "Choose the track again to continue playback."
                return
            }
            if reference.kind == .track {
                startSpotifySelection(item: item, collection: nil,
                    resume: RestoredRemote(contextUri: spotifyState?.playback?.contextUri),
                    positionMs: snapshot?.positionMs ?? 0)
            } else {
                startSpotifySelection(item: item, collection: nil, positionMs: snapshot?.positionMs ?? 0)
            }
        } else {
            submitPlaybackToggle(play: !playbackToggleShowsPause)
        }
    }
    @discardableResult
    private func submitPlaybackToggle(play: Bool) -> UInt64 {
        let owned = ownsPlaybackControl
        let request = send(play ? .play : .pause)
        if owned {
            pendingOwnedPlayback = (request, play, ProcessInfo.processInfo.systemUptime + 2, isPlaying != play)
        }
        return request
    }
    func stop() { send(.stop) }
    func next() {
        guard !isRestarting else { return }
        var projection: SpotifyNextProjection?
        if let snapshot, snapshot.activeProvider == "spotify", let device = spotifyState?.playback?.device,
           device.isActive, !device.isRestricted {
            if pendingSkip == nil {
                projection = SpotifyNextProjection(device: device.id)
            } else if var prior = pendingSkip?.spotifyNext, prior.device == device.id {
                prior.remaining = min(prior.remaining + 1, SpotifyNextProjection.limit + 1)
                projection = prior
            }
            projection?.observeQueue(snapshot.queue)
        }
        skip(.next, preview: nil, direction: 1, available: canGoNext, projection: projection,
             spotifyReturn: spotifyReturnAfterPrevious())
    }
    private func spotifyReturnAfterPrevious() -> PendingSkip.SpotifyReturn? {
        guard ownsPlaybackControl, let prior = pendingSkip, let predecessor = prior.previousPreview,
              let item = current, item.id == prior.currentID,
              !SpotifyPlaybackIdentity.matches(item.id, predecessor.id),
              let context = spotifyState?.playback?.contextUri,
              let clock = spotifyPlayback.clock, clock.active, let epoch = clock.queueEpoch, epoch > 0,
              SpotifyPlaybackIdentity.matches(clock.mediaId, item.id) else { return nil }
        return .init(previousRequest: prior.request, item: item, predecessorID: predecessor.id,
                     contextURI: context, outgoingEpoch: epoch)
    }
    func previous() {
        guard !isRestarting else { return }
        if hasPendingLocalSeek {
            // The serial coordinator has the accepted seek before Previous runs.
            if canSeek { send(.previous) }
        } else if previousRestartsTrack {
            seek(milliseconds: 0)
        } else {
            guard canGoToPreviousTrack else { return }
            let preview = pendingSkip == nil && snapshot?.activeProvider == "spotify" ? snapshot?.queue.deviceQueue?.previous.first : nil
            skip(.skipPrevious, preview: preview, direction: -1, available: true)
        }
    }
    private func skip(_ command: PlayerCommand, preview: MediaItem?, direction: Int, available: Bool,
                      projection: SpotifyNextProjection? = nil, spotifyReturn: PendingSkip.SpotifyReturn? = nil) {
        let baseline = snapshot
        let followingSkip = pendingSkip != nil
        let nextIsLocal = baseline?.queue.upcoming.first?.capabilities.pcmAnalysis ?? (baseline?.queue.repeatMode == .all)
        let localPlay = hasLocalPCMOutput && (direction < 0 || nextIsLocal)
            ? localPlaybackToggleIntent ?? (isPlaying || baseline?.state == .paused ? isPlaying : nil)
            : nil
        let ownedNext: (device: String, generation: UInt64)? = if available, case .next = command,
            ownsPlaybackControl, let device = ownedSpotifyDevice {
            (device.id, spotifyPlayback.clockGeneration)
        } else { nil }
        let ownedSeekBeforePrevious: PendingOwnedSeek? = if case .skipPrevious = command, let seek = pendingOwnedSeek,
            matchingOwnedSeekClock(seek) != nil { seek } else { nil }
        var spatialPrevious: SpatialPreviousConfirmation? = if case .skipPrevious = command, available, !followingSkip {
            spatialPreviousOrigin()
        } else { nil }
        let request = send(command)
        if Self.tracePlayback, case .next = command, baseline?.activeProvider == "spotify" {
            Self.playbackTiming.info("navigation_next_store request=\(request, privacy: .public) available=\(available, privacy: .public) following_skip=\(followingSkip, privacy: .public) bridge_ready=\(self.bridge != nil && !self.bridgeFailed && !self.isClosing, privacy: .public)")
        }
        guard available, !bridgeFailed, !isClosing, let baseline else { return }
        spatialPrevious?.request = request
        if let spatialPrevious { spatialPreviousConfirmation = spatialPrevious }
        if Self.tracePlayback, ownedNext != nil {
            Self.playbackTiming.info("owned_next_requested request=\(request, privacy: .public)")
        }
        trackTransitionDirection = direction
        pendingSkip = PendingSkip(request: request, provider: baseline.activeProvider, currentID: baseline.queue.current?.id,
                                  positionMs: baseline.positionMs, previousPreview: preview, spotifyNext: projection,
                                  localPlayback: localPlay.map { ($0, nil) }, ownedNext: ownedNext,
                                  ownedSeekBeforePrevious: ownedSeekBeforePrevious,
                                  spotifyReturn: ownedNext == nil ? nil : spotifyReturn)
        if Self.tracePlayback, pendingSkip?.spotifyReturn != nil {
            Self.playbackTiming.info("owned_return_projection phase=pending request=\(request, privacy: .public)")
        }
        updateArtworkPalette(); prewarmNextArtwork()
        pendingSkipTimeout = Task { [weak self] in
            do { try await Task.sleep(for: .seconds(3)) } catch { return }
            guard let self, pendingSkip?.request == request else { return }
            clearPendingSkip()
        }
    }
    private func clearPendingSkip(keepDirection: Bool = false) {
        guard let skip = pendingSkip else { return }
        pendingSkipTimeout?.cancel(); pendingSkipTimeout = nil; pendingSkip = nil
        spatialNextOrigin = nil
        if !keepDirection { trackTransitionDirection = 0 }
        if skip.spotifyReturn != nil {
            if Self.tracePlayback { Self.playbackTiming.info("owned_return_projection phase=ended request=\(skip.request, privacy: .public)") }
            lyrics.select(current)
        }
        updateArtworkPalette(); prewarmNextArtwork()
    }
    private func observesSpotifyReturn(_ skip: PendingSkip, _ returning: PendingSkip.SpotifyReturn) -> Bool {
        guard !spotifyReadRevoked, let remote = spotifyState, remote.ownedOutputObserved == true, remote.observesPlayback,
              let scope = skip.ownedNext, scope.generation == spotifyPlayback.clockGeneration,
              let device = remote.playback?.device, device.id == scope.device, device.id == ownedSpotifyDevice?.id,
              device.isActive, !device.isRestricted, spotifyOutputSelectionAllows(device.id) else { return false }
        return (spotifyPlayback.clock?.metadata?.contextUri ?? remote.playback?.contextUri) == returning.contextURI
    }
    private func reconcileSpotifyReturn(_ skip: PendingSkip, _ returning: PendingSkip.SpotifyReturn, state: PlaybackSnapshot) {
        let request = skip.request
        guard state.activeProvider == "spotify", observesSpotifyReturn(skip, returning),
              [.loading, .playing, .paused].contains(state.state), let item = state.queue.current else {
            if Self.tracePlayback { Self.playbackTiming.info("owned_return_projection phase=invalid_snapshot request=\(request, privacy: .public) state=\(state.state.rawValue, privacy: .public)") }
            clearPendingSkip()
            return
        }
        let clock = spotifyPlayback.clock
        let epoch = clock?.active == true && SpotifyPlaybackIdentity.matches(clock?.mediaId, item.id) ? clock?.queueEpoch : nil
        if SpotifyPlaybackIdentity.matches(item.id, returning.predecessorID) {
            if returning.predecessorEpoch == nil, let epoch, epoch > returning.outgoingEpoch {
                pendingSkip?.spotifyReturn?.predecessorEpoch = epoch
                if Self.tracePlayback { Self.playbackTiming.info("owned_return_projection phase=held request=\(request, privacy: .public)") }
            }
            return
        }
        guard SpotifyPlaybackIdentity.matches(item.id, returning.item.id) else {
            if Self.tracePlayback { Self.playbackTiming.info("owned_return_projection phase=different_item request=\(request, privacy: .public)") }
            clearPendingSkip()
            return
        }
        if let epoch, let predecessor = returning.predecessorEpoch, epoch > predecessor,
           state.state == .playing || state.state == .paused {
            if Self.tracePlayback { Self.playbackTiming.info("owned_return_projection phase=completed request=\(request, privacy: .public)") }
            clearPendingSkip(keepDirection: true)
        }
    }
    private func reconcileSkip(_ state: PlaybackSnapshot) {
        guard var skip = pendingSkip else { return }
        if let returning = skip.spotifyReturn {
            reconcileSpotifyReturn(skip, returning, state: state)
            return
        }
        if state.activeProvider == "spotify", [.playing, .paused].contains(state.state), hasPendingOwnedNext,
           spotifyState?.confirmedSameMediaNextRequestId == skip.request,
           SpotifyPlaybackIdentity.matches(state.queue.current?.id, spotifyState?.playback?.current?.id) {
            if Self.tracePlayback {
                Self.playbackTiming.info("owned_next_same_media_confirmed request=\(skip.request, privacy: .public)")
            }
            let changed = state.queue.current?.id != skip.currentID
            let predicted = skip.preview == nil || skip.preview?.id == state.queue.current?.id
            clearPendingSkip(keepDirection: changed && predicted)
            return
        }
        if let play = localHandoffPlayIntent {
            if state.state == .loading { return }
            if let request = skip.localPlayback?.request,
               pendingCommands.contains(request) || state.state == (play ? .paused : .playing) { return }
        }
        let changed = state.queue.current?.id != skip.currentID
        if state.activeProvider == skip.provider, var projection = skip.spotifyNext {
            guard let device = spotifyState?.playback?.device, device.id == projection.device, device.isActive, !device.isRestricted else {
                clearPendingSkip()
                return
            }
            if changed {
                if [.loading, .playing, .paused].contains(state.state), let id = state.queue.current?.id,
                   let index = projection.prefix.firstIndex(where: { $0.id == id }), index + 1 < projection.remaining {
                    projection.remaining -= index + 1
                    projection.prefix.removeFirst(index + 1)
                    if state.queue.deviceQueue != nil { projection.observeQueue(state.queue) }
                    skip.currentID = id
                    skip.positionMs = state.positionMs
                    skip.spotifyNext = projection
                    pendingSkip = skip
                    return
                }
            } else {
                projection.observeQueue(state.queue)
                skip.spotifyNext = projection
                pendingSkip = skip
            }
        }
        // A late seek or an earlier occurrence can reset the clock while a newer Next is pending.
        let protectsOwnedNext = hasPendingOwnedNext && (
            skip.preview.map { $0.id != skip.currentID } == true
            || (skip.spotifyNext?.remaining ?? 1) > 1
            || spotifyState?.confirmedSameMediaNextRequestId != nil
        )
        // The rewind from the preceding seek still belongs to the old track.
        let protectsOwnedPrevious = skip.ownedSeekBeforePrevious.map {
            matchingOwnedSeekClock($0) != nil && Double(state.positionMs) >= Double($0.positionMs) - 250
        } == true
        let awaitingOwnedPrevious = skip.ownedPrevious.map {
            $0.device == ownedSpotifyDevice?.id && $0.generation == spotifyPlayback.clockGeneration
        } == true
        let positionRestarted = skip.positionMs > state.positionMs && skip.positionMs - state.positionMs >= 500
        if state.activeProvider != skip.provider || changed || state.state == .error || state.state == .stopped
            || (positionRestarted && !protectsOwnedNext && !protectsOwnedPrevious && !awaitingOwnedPrevious) {
            let predicted = skip.preview == nil || skip.preview?.id == state.queue.current?.id
            clearPendingSkip(keepDirection: changed && state.activeProvider == skip.provider && predicted)
        }
    }
    @discardableResult
    func seek(seconds: Double) -> UInt64? {
        guard seconds.isFinite, let current else { return nil }
        let bounded = min(max(0, seconds), Double(current.durationMs) / 1000)
        // Conversion remains safe for the entire UInt64 domain of a decoded duration.
        let milliseconds = (bounded * 1000).rounded()
        return seek(milliseconds: milliseconds >= Double(UInt64.max) ? UInt64.max : UInt64(milliseconds))
    }
    @discardableResult
    func seek(milliseconds: UInt64) -> UInt64? {
        guard canSeek, let current else { return nil }
        return send(.seek(positionMs: min(milliseconds, current.durationMs)))
    }
    func setVolume(_ value: Double) {
        guard value.isFinite, (0...1).contains(value), !bridgeFailed, !isClosing, !isRestarting else { return }
        let device = volumeDeviceID
        if value > 0 { volumeDefaults.set(value, forKey: Self.audibleVolumeKey) }
        if let device, device == ownedSpotifyDevice?.id, selectedSpotifyDeviceID == nil || selectedSpotifyDeviceID == device {
            spotifyPlayback.rememberVolume(value)
        }
        clearPendingVolume()
        let request = nextRequestId
        pendingVolume = PendingVolume(request: request, value: value, provider: snapshot?.activeProvider, device: device)
        send(.setVolume(Float(value)))
        if pendingVolume?.request == request {
            pendingVolumeTimeout = Task { [weak self] in
                do { try await Task.sleep(for: .seconds(2)) } catch { return }
                guard let self, pendingVolume?.request == request else { return }
                clearPendingVolume()
            }
        }
    }
    func toggleMute() {
        guard snapshot?.capabilities.volume == true else { return }
        if isMuted {
            let remembered = volumeDefaults.double(forKey: Self.audibleVolumeKey)
            setVolume(remembered > 0 && remembered <= 1 ? remembered : 0.4)
        } else {
            volumeDefaults.set(volume, forKey: Self.audibleVolumeKey)
            setVolume(0)
        }
    }
    private func clearPendingVolume() {
        pendingVolumeTimeout?.cancel(); pendingVolumeTimeout = nil; pendingVolume = nil
    }
    private func reconcileVolume() {
        guard let draft = pendingVolume, let snapshot else { return }
        let tolerance = draft.provider == "spotify" ? 0.0051 : 0.000_01
        if draft.provider != snapshot.activeProvider || draft.device != volumeDeviceID || abs(draft.value - Double(snapshot.volume)) <= tolerance {
            clearPendingVolume()
        }
    }
    private var queueModeScope: QueueModeScope? {
        guard let snapshot, snapshot.queue.current != nil || !snapshot.queue.upcoming.isEmpty else { return nil }
        let remote = snapshot.activeProvider == "spotify"
        return QueueModeScope(provider: snapshot.activeProvider, device: volumeDeviceID,
                              context: remote ? spotifyState?.playback?.contextUri : nil)
    }
    private func trackQueueMode(_ command: PlayerCommand, request: UInt64) {
        let kind: QueueModeKind
        switch command {
        case .setShuffle(let value):
            guard let scope = queueModeScope, let queue = snapshot?.queue else { return }
            kind = .shuffle
            pendingShuffle = PendingQueueMode(request: request, value: value, scope: scope, sawDifferent: value != queue.shuffle)
        case .setRepeat(let value):
            guard let scope = queueModeScope, let queue = snapshot?.queue else { return }
            kind = .repeatMode
            pendingRepeat = PendingQueueMode(request: request, value: value, scope: scope, sawDifferent: value != queue.repeatMode)
        default: return
        }
        queueModeTimeouts[kind]?.cancel()
        queueModeTimeouts[kind] = Task { [weak self] in
            do { try await Task.sleep(for: .seconds(3)) } catch { return }
            self?.clearQueueMode(kind)
        }
    }
    private func clearQueueMode(_ kind: QueueModeKind) {
        queueModeTimeouts.removeValue(forKey: kind)?.cancel()
        switch kind {
        case .shuffle: pendingShuffle = nil
        case .repeatMode: pendingRepeat = nil
        }
    }
    private func clearRejectedQueueModes(_ request: UInt64?) {
        if request == nil || pendingShuffle?.request == request { clearQueueMode(.shuffle) }
        if request == nil || pendingRepeat?.request == request { clearQueueMode(.repeatMode) }
    }
    private func reconcileQueueModes() {
        let scope = queueModeScope
        if var draft = pendingShuffle {
            let wasDifferent = draft.sawDifferent
            if draft.reconcile(snapshot?.queue.shuffle ?? false, scope: scope) { clearQueueMode(.shuffle) }
            else if draft.sawDifferent != wasDifferent { pendingShuffle = draft }
        }
        if var draft = pendingRepeat {
            let wasDifferent = draft.sawDifferent
            if draft.reconcile(snapshot?.queue.repeatMode ?? .off, scope: scope) { clearQueueMode(.repeatMode) }
            else if draft.sawDifferent != wasDifferent { pendingRepeat = draft }
        }
    }
    func seekRelative(seconds: Double) {
        guard seconds.isFinite else { return }
        seek(seconds: min(max(0, position() + seconds), Double(current?.durationMs ?? 0) / 1000))
    }
    func adjustVolume(_ delta: Double) {
        guard delta.isFinite, snapshot?.capabilities.volume == true else { return }
        setVolume(min(max(0, volume + delta), 1))
    }
    func attachSystemMedia(_ session: any SystemMediaSession) {
        if systemMedia === session { return }
        systemMedia?.clear(); systemMedia = session
        if let snapshot { session.publish(snapshot) }
    }
    /// A native output lease augments the remote provider's capabilities only in
    /// this host. External Connect devices never acquire macOS media ownership.
    func ownsSystemOutput(_ state: PlaybackSnapshot) -> Bool {
        hasOwnedOutput(state, allowingBuffering: false)
    }
    private func hasOwnedOutput(_ state: PlaybackSnapshot, allowingBuffering: Bool) -> Bool {
        guard unclaimedSpotifyActivation == nil, !defersRestoredOutput(state), !isSpotifyContextItemStart || spotifyStartHasConfirmedFrame,
              let pid = spotifyPlayback.status.processID, ownedOutputSession == pid,
              spotifyPlayback.status.authenticated == true,
              !spotifyPlayback.status.clockSupported || spotifyPlayback.clock?.isValid(at: ProcessInfo.processInfo.systemUptime) == true,
              spotifyPlayback.status.active != false,
              spotifyPlayback.clock?.mediaId == nil || SpotifyPlaybackIdentity.matches(spotifyPlayback.clock?.mediaId, state.queue.current?.id),
              spotifyPlayback.clock.map({ [.playing, .paused].contains($0.state) || (allowingBuffering && $0.state == .loading) }) != false,
              spotifyStartingItemID == nil || SpotifyPlaybackIdentity.matches(spotifyStartingItemID, state.queue.current?.id),
              state.activeProvider == "spotify", spotifyState?.observesPlayback == true,
              let device = ownedSpotifyDevice,
              spotifyOutputSelectionAllows(device.id),
              spotifyState?.playback?.device?.id == device.id,
              spotifyState?.playback?.current?.id == state.queue.current?.id else { return false }
        return true
    }
    private func publishSystemMedia() {
        if isRestarting {
            ownedOutputSession = nil; ownedDeviceContinuation = nil
            systemMedia?.clear()
            clearHostAnalysis(intent: false)
            return
        }
        guard let snapshot else { return }
        if unclaimedSpotifyActivation != nil || defersRestoredOutput(snapshot) {
            ownedOutputSession = nil
        } else if let pid = spotifyPlayback.status.processID,
           spotifyPlayback.status.authenticated == true,
           !spotifyPlayback.status.clockSupported || spotifyPlayback.clock?.isValid(at: ProcessInfo.processInfo.systemUptime) == true,
           spotifyPlayback.status.active != false,
           spotifyPlayback.clock?.mediaId == nil || SpotifyPlaybackIdentity.matches(spotifyPlayback.clock?.mediaId, snapshot.queue.current?.id),
           spotifyPlayback.clock == nil || spotifyPlayback.clock?.state == .playing,
           snapshot.activeProvider == "spotify", snapshot.state == .playing,
           spotifyState?.observesPlayback == true, spotifyState?.playback?.state == .playing,
           let device = ownedSpotifyDevice, spotifyState?.playback?.device?.id == device.id,
           spotifyState?.playback?.current?.id == snapshot.queue.current?.id,
           spotifyOutputSelectionAllows(device.id) {
            ownedOutputSession = pid
        } else if snapshot.activeProvider != "spotify" || spotifyPlayback.status.processID != ownedOutputSession || spotifyPlayback.status.active == false || (spotifyPlayback.status.clockSupported && spotifyPlayback.clock?.isValid(at: ProcessInfo.processInfo.systemUptime) != true) || spotifyPlayback.clock?.state == .idle || spotifyState?.playback?.device?.id != ownedSpotifyDevice?.id {
            ownedOutputSession = nil
        }
        rememberOwnedSpotifyDevice()
        systemMedia?.publish(snapshot)
        forwardHostAnalysis()
    }
    private func defersRestoredOutput(_ state: PlaybackSnapshot) -> Bool {
        guard state.activeProvider == "spotify" else { return false }
        if spotifyStartRestoresSession && spotifyStartPauseRequested { return true }
        if canApplyTimedOutSpotifyPause && spotifyStartHasConfirmedFrame { return true }
        guard let pause = restoredPauseOutput,
              ProcessInfo.processInfo.systemUptime < pause.expires,
              spotifyPlayback.clockGeneration == pause.runtime,
              state.queue.current?.id == pause.media,
              spotifyState?.playback?.device?.id == pause.device else { return false }
        return true
    }
    func toggleShuffle() { send(.setShuffle(!shuffleEnabled)) }
    func cycleRepeat() {
        let next: RepeatMode = switch repeatMode ?? .off { case .off: .all; case .all: .one; case .one: .off }
        send(.setRepeat(next))
    }
    func playNext(_ item: MediaItem) { send(.playNext(item)) }
    func jumpQueue(_ index: Int) { guard let queue = snapshot?.queue else { return }; send(.jumpQueue(revision: queue.revision, index: index)) }
    func removeQueueItem(_ index: Int) { guard let queue = snapshot?.queue else { return }; send(.removeQueueItem(revision: queue.revision, index: index)) }
    func clearQueue() { send(.clearQueue) }
    func prepareToQuit() async {
        clearHostAnalysis(intent: true)
        isClosing = true; lyrics.cancel()
        spotifyPlayback.changed = nil
        spotifyPlayback.observed = nil
        spotifyPlayback.interrupted = nil
        spotifyPlayback.resetting = nil
        pendingOutputScopes.removeAll(); acceptedOutputScope = nil
        localLoadingSeekIntent = nil; pendingOwnedSeek = nil; pendingLocalPlayback = nil
        cancelSpotifyStart(); clearSpotifyTransfer(); clearPendingSkip(); clearPendingVolume(); clearRejectedQueueModes(nil)
        artworkTask?.cancel(); artworkTask = nil
        nextArtworkTask?.cancel(); nextArtworkTask = nil; nextArtworkTarget = nil
        await initialRestoreTask?.value; initialRestoreTask = nil
        bridgeEpoch &+= 1
        analysisObservation = nil; publishVisualModulation()
        spotifyAuth?.shutdown(); spotifyAuth = nil
        systemMedia?.clear(); systemMedia = nil
        // Persist the current playback position before terminating the source
        // that backs the coordinator's live position.
        await withCheckedContinuation { continuation in
            guard let bridge else { continuation.resume(); return }
            bridge.close { continuation.resume() }
        }
        await spotifyPlayback.shutdown()
    }
    private func apply(_ event: PlayerEvent) {
        switch event {
        case .audioAnalysis(let observation):
            if observation.mediaId != analysisObservation?.mediaId || observation.frame?.generation != analysisObservation?.frame?.generation || observation.frame?.windowEndFrame != analysisObservation?.frame?.windowEndFrame {
                analysisReceivedAt = ProcessInfo.processInfo.systemUptime
            }
            analysisObservation = observation
        case .remoteOutputCommand(let provider, let command):
            let epoch = bridgeEpoch
            let report: @MainActor (Bool) -> Void = { [weak self] accepted in
                guard let self, bridgeEpoch == epoch, !isClosing, !bridgeFailed else { return }
                if command.control.kind == .startUri, let operation = command.control.operationId {
                    Self.playbackTiming.info("selection_local_result operation=\(operation, privacy: .public) accepted=\(accepted, privacy: .public)")
                }
                if Self.tracePlayback, command.control.kind == .previousFromStart {
                    Self.playbackTiming.info("owned_previous_local_result id=\(command.id, privacy: .public) accepted=\(accepted, privacy: .public)")
                }
                send(.remote(provider: provider, action: .outputCommandResult(RemoteOutputCommandResult(id: command.id, generation: command.generation, accepted: accepted))))
                if !accepted, let request = command.requestId, request == restoredPauseOutput?.request {
                    restoredPauseOutput = nil
                    if pendingOwnedPlayback?.request == request { pendingOwnedPlayback = nil }
                    publishSystemMedia()
                }
                if self.spatialSeekConfirmation?.dispatch?.id == command.id,
                   self.spatialSeekConfirmation?.seek.request == command.requestId {
                    if accepted {
                        self.spatialSeekConfirmation?.accepted = true
                        self.forwardHostAnalysis()
                    } else {
                        self.spatialSeekConfirmation = nil
                        self.send(.hostAnalysis(nil), outputUpdate: true)
                    }
                }
                if let proof = spatialPreviousConfirmation, command.control.kind == .previousFromStart,
                   proof.request == command.requestId, proof.runtime == command.generation, proof.dispatch?.id == command.id {
                    if accepted {
                        spatialPreviousConfirmation?.accepted = true
                        forwardHostAnalysis()
                    } else {
                        spatialPreviousConfirmation = nil
                    }
                }
                if command.control.kind == .next || command.control.kind == .previous || command.control.kind == .previousFromStart {
                    traceSpatialRelative("ack", reason: accepted ? "accepted" : "rejected", proof: spatialRelativeConfirmation, command: command)
                }
                if let proof = spatialRelativeConfirmation, proof.request == command.requestId,
                   proof.runtime == command.generation, proof.dispatch?.id == command.id {
                    if accepted {
                        spatialRelativeConfirmation?.accepted = true
                        forwardHostAnalysis()
                    } else {
                        spatialRelativeConfirmation = nil
                    }
                }
            }
            guard provider == "spotify", !isClosing, !bridgeFailed, !spotifyReadRevoked,
                  spotifyState?.ownedOutputControl == true || (command.control.kind.isOutputTransport && spotifyState?.ownedOutputObserved == true),
                  ownedSpotifyDevice?.id == command.deviceId,
                  spotifyOutputSelectionAllows(command.deviceId) else {
                traceSpatialRelative("dispatch_unbound", reason: "host_gate", proof: spatialRelativeConfirmation, command: command)
                report(false); return
            }
            observeSpatialRelativeDispatch(command)
            if command.control.kind == .previousFromStart,
               pendingSkip?.provider == provider, trackTransitionDirection < 0,
               SpotifyPlaybackIdentity.matches(pendingSkip?.currentID, command.mediaId) {
                self.pendingSkip?.ownedPrevious = (command.deviceId, command.generation)
            }
            if command.control.kind == .previousFromStart { observeSpatialPreviousDispatch(command) }
            if command.control.kind == .next, let request = command.requestId,
               let skip = pendingSkip, skip.request == request, let returning = skip.spotifyReturn,
               skip.ownedNext?.generation == command.generation, let epoch = command.requiredQueueEpoch, epoch > returning.outgoingEpoch,
               SpotifyPlaybackIdentity.matches(command.mediaId, returning.predecessorID) {
                pendingSkip?.spotifyReturn?.predecessorEpoch = epoch
            }
            if command.control.kind == .autoplayEnqueue {
                guard snapshot?.activeProvider == provider, snapshot?.state == .playing,
                      pendingSpotifyStart == nil, pendingSpotifyTransfer == nil, pendingSkip == nil else {
                    report(false); return
                }
            }
            if command.control.kind == .startUri {
                guard !isSpotifyContextItemStart,
                      let operation = command.control.operationId, operation == spotifyStartOperationID,
                      let observation = spotifyState?.observationId else { report(false); return }
                let frame = spotifyState?.playback
                let wasPlayingTarget = frame.map { $0.state == .playing && matchesSpotifyStart($0) } == true
                spotifyStartLocalBaseline = (observation, command.generation, wasPlayingTarget)
                Self.playbackTiming.info("selection_local_dispatch operation=\(operation, privacy: .public)")
            }
            if command.control.kind == .seek, let proof = spatialSeekConfirmation,
               command.requestId == proof.seek.request, command.control.positionMs == proof.seek.positionMs,
               command.deviceId == proof.seek.deviceID, command.generation == proof.seek.generation,
               command.mediaId == proof.seek.mediaID, command.sequence >= proof.seek.sequence,
               matchingOwnedSeekClock(proof.seek) != nil, proof.processID == spotifyPlayback.status.processID {
                spatialSeekConfirmation?.dispatch = (command.id, command.sequence,
                    ProcessInfo.processInfo.systemUptime, Date().timeIntervalSince1970 * 1000)
            }
            if command.control.kind == .play, command.requiredQueueEpoch != nil {
                spotifyPlayback.send(command, ownedDeviceID: ownedSpotifyDevice?.id, admission: { [weak self] in
                    self?.allowsOwnedPreviousRecovery(command) == true
                }, completion: report)
            } else {
                spotifyPlayback.send(command, ownedDeviceID: ownedSpotifyDevice?.id, completion: report)
            }
        case .remoteState(let state):
            let previous = Self.tracePlayback ? spotifyState : nil
            let hadOwnedDevice = Self.tracePlayback && ownedSpotifyDevice != nil
            remoteStates[state.provider] = state
            spotifyState = state
            if let pause = restoredPauseOutput, state.playback?.device?.id == pause.device,
               state.playback?.current?.id == pause.media, state.playback?.state == .paused {
                restoredPauseOutput = nil
            }
            if Self.tracePlayback {
                let hasOwnedDevice = ownedSpotifyDevice != nil
                let hasPlayback = state.playback != nil
                if previous?.playback?.state != state.playback?.state || hadOwnedDevice != hasOwnedDevice
                    || previous?.connection != state.connection || previous?.authenticationRequired != state.authenticationRequired {
                    Self.playbackTiming.info("output_visibility playback=\(hasPlayback, privacy: .public) known_device=\(hasOwnedDevice, privacy: .public) owned_observation=\(state.ownedOutputObserved == true, privacy: .public) connection=\(state.connection.rawValue, privacy: .public) auth_required=\(state.authenticationRequired, privacy: .public)")
                }
            }
            if pendingSkip?.provider == "spotify", state.authenticationRequired || !state.observesPlayback { clearPendingSkip() }
            if let skip = pendingSkip, let returning = skip.spotifyReturn,
               !observesSpotifyReturn(skip, returning) {
                if Self.tracePlayback { Self.playbackTiming.info("owned_return_projection phase=scope_lost request=\(skip.request, privacy: .public)") }
                clearPendingSkip()
            }
            if pendingVolume?.provider == "spotify", state.authenticationRequired || pendingVolume?.device != volumeDeviceID { clearPendingVolume() }
            if state.authenticationRequired && snapshot?.activeProvider == "spotify" { clearRejectedQueueModes(nil) }
            if let draft = pendingRepeat, draft.scope != queueModeScope { clearQueueMode(.repeatMode) }
            if let draft = pendingShuffle, draft.scope != queueModeScope { clearQueueMode(.shuffle) }
            reconcileSpotifyTransfer(state)
            forwardOutputClock()
            if let result = state.controlResult, result.operationId == spotifyStartOperationID {
                Self.playbackTiming.info("selection_provider_result operation=\(result.operationId, privacy: .public) accepted=\(result.error == nil, privacy: .public)")
                if let issue = result.error {
                    if spotifyStartLocalBaseline != nil, issue.code == "provider_unavailable" {
                        // A lost local receipt does not establish whether the write ran.
                        spotifyStartUncertain = true
                    } else {
                        cancelSpotifyStart(); errorMessage = issue.message
                    }
                } else { spotifyStartAccepted = true }
            }
            publishSystemMedia()
            spotifyPlayback.noteDeviceAvailable(ownedSpotifyDevice != nil)
            if ownedSpotifyDevice != nil && errorMessage == Self.playbackSetupIssue { errorMessage = nil }
            if state.authenticationRequired {
                spotifyStartPauseRequested = false
                if spotifyEditPending { spotifyEditIssue = "The connection expired before Spotify confirmed the update. Check your library before retrying." }
                spotifyReadRevoked = true; spotifyPage = nil
                prewarmNextArtwork()
                if !spotifyDetails.isEmpty || !spotifyDetailQueries.isEmpty || !spotifySaved.isEmpty || !spotifyCursorPages.isEmpty || spotifyCreatedPlaylist != nil { clearSpotifyReadState() }
                spotifyEditPending = false; spotifyEditTicket = nil; spotifyAuth?.unauthorized()
            }
            if state.connection == .connected {
                spotifyAuth?.noteConnected()
                if !spotifyReadRevoked { inspectSpotify(.profile) }
                if spotifyPage == nil && spotifyQuery.queryId == 0 { querySpotify() }
            }
        case .remoteEdited(let result):
            if !spotifyReadRevoked, spotifyEditPending, result.operationId == spotifyOperationID {
                spotifyEditPending = false; spotifyEditTicket = nil
                spotifyEditIssue = result.error?.message
                spotifyEditSuccess = result.error == nil ? (result.saved?.first.map { $0.saved ? "Saved in your library" : "Not saved in your library" } ?? "Library updated") : nil
                spotifyCreatedPlaylist = result.collection
                if result.error == nil, let edit = spotifySubmittedEdit, edit.kind == "playlist_details", let id = edit.id, let name = edit.name {
                    spotifyCreatedPlaylist = RemoteCollection(id: id, title: name, url: "https://open.spotify.com/playlist/\(id)", artwork: nil, isPublic: edit.public, kind: .playlist, description: edit.description, collaborative: edit.collaborative)
                }
                let completedEdit = spotifySubmittedEdit; spotifySubmittedEdit = nil
                if result.error?.code == "catalog_changed", let edit = completedEdit, edit.kind == "playlist_move",
                   spotifyQuery.kind == .playlistItems, spotifyQuery.text == edit.id, spotifyQuery.queryId == edit.queryId {
                    querySpotify(offset: spotifyQuery.offset)
                }
                if result.error == nil, let edit = completedEdit, let id = edit.id, ["playlist_details", "playlist_cover"].contains(edit.kind) {
                    invalidateSpotifyDetail(.playlistInfo, text: id); inspectSpotify(.playlistInfo, text: id, refresh: true)
                    if edit.kind == "playlist_cover" { invalidateSpotifyDetail(.playlistImages, text: id); inspectSpotify(.playlistImages, text: id, refresh: true) }
                }
                if result.error == nil, let edit = completedEdit,
                   ["create_playlist", "playlist_details", "playlist_cover"].contains(edit.kind)
                    || (edit.kind == "library" && (edit.uris ?? []).contains { $0.hasPrefix("spotify:playlist:") }) {
                    invalidateSpotifyDetail(.homePlaylists, text: "")
                    homePlaylistRevision &+= 1
                }
                if result.error == nil && result.saved?.isEmpty != false { querySpotify(offset: completedEdit?.kind == "playlist_replace" ? 0 : spotifyQuery.offset) }
                if result.error == nil {
                    // A response already requested, including the automatic reload
                    // after a write, may still contain the old membership.
                    let reported = result.saved ?? []
                    for item in reported { rememberSpotifySaved(item.uri, saved: item.saved, throughQuery: spotifyQueryID) }
                    if reported.isEmpty, completedEdit?.kind == "library", let saved = completedEdit?.save {
                        for uri in completedEdit?.uris ?? [] { rememberSpotifySaved(uri, saved: saved, throughQuery: spotifyQueryID) }
                    }
                }
            }
        case .remotePage(let page):
            guard page.provider == "spotify", !spotifyReadRevoked, !isClosing, !bridgeFailed else { break }
            if page.provider == "spotify", spotifyDetailQueries[page.queryId] != nil { acceptSpotifyDetail(page) }
            else if page.detail != true, page.queryId == spotifyQuery.queryId {
                spotifyPage = page
                observeSpotifySavedPage(page)
            }
        case .libraryState(let state): applyLibraryState(state)
        case .libraryPage(let page):
            if page.queryId == libraryQueryId {
                libraryPage = page
                if page.offset > 0 && page.total > 0 && page.tracks.isEmpty && page.albums.isEmpty {
                    libraryQuery.offset = UInt32((page.total - 1) / UInt64(libraryQuery.limit)) * libraryQuery.limit
                    requestLibraryPage()
                }
            }
        case .catalog(let items): tracks = items
        case .mediaUpdated(let item):
            if let index = tracks.firstIndex(where: { $0.id == item.id }) { tracks[index] = item }
            else { tracks.append(item) }
        case .snapshot(let state):
            if state.revision >= (snapshot?.revision ?? 0) {
                if Self.tracePlayback, state.activeProvider == "spotify",
                   (snapshot?.queue.current != nil) != (state.queue.current != nil)
                    || (snapshot?.restoredRemote != nil) != (state.restoredRemote != nil) {
                    Self.playbackTiming.info("output_selection present=\(state.queue.current != nil, privacy: .public) restored=\(state.restoredRemote != nil, privacy: .public) play=\(state.capabilities.play, privacy: .public) state=\(state.state.rawValue, privacy: .public)")
                }
                if pendingSkip == nil && state.queue.current?.id != current?.id { trackTransitionDirection = 0 }
                if analysisObservation != nil, state.state != .playing || !state.capabilities.pcmAnalysis || state.queue.current?.id != current?.id { analysisObservation = nil }
                if let revoked = state.hostAnalysisRevokedThrough { reconcileHostAnalysisRevocation(revoked) }
                if soundDraft != nil, state.sound?.settings == soundDraft { soundDraft = nil; soundRequest = nil }
                observeSpotifySpokenPlayback(state)
                snapshot = state
                if let intent = localLoadingSeekIntent,
                   !localSeekAppliesToCurrentTrack || state.state == .error || state.state == .idle
                    || (state.state != .loading && !pendingCommands.contains(intent.request)) {
                    localLoadingSeekIntent = nil
                }
                if let intent = pendingLocalPlayback,
                   state.activeProvider != intent.provider || current?.id != intent.mediaID || !hasLocalPCMOutput
                    || state.state == .error || state.state == .idle
                    || (!pendingCommands.contains(intent.request) && state.state != .loading) {
                    // The receipt precedes the post-command snapshot. Keep the
                    // latest button intent through earlier snapshots and loading.
                    pendingLocalPlayback = nil
                }
                reconcileSkip(state)
                lyrics.select(pendingSkip?.spotifyReturn?.item ?? state.queue.current)
                if let pending = pendingOwnedPlayback {
                    if !hasPendingOwnedPlayback || (!ownedRelativeHandoff && pending.sawOpposite
                        && state.state == (pending.play ? .playing : .paused)) {
                        pendingOwnedPlayback = nil
                    } else if state.state == (pending.play ? .paused : .playing) {
                        pendingOwnedPlayback?.sawOpposite = true
                    }
                }
                reconcileVolume()
                reconcileQueueModes()
                if state.restoredRemote != nil, let item = state.queue.current, let id = item.id.split(separator: ":").last.map(String.init) {
                    let kind: RemoteCatalogKind = item.id.hasPrefix("spotify:episode:") ? .episode : item.id.hasPrefix("spotify:chapter:") ? .chapter : .track
                    inspectSpotify(kind, text: id)
                }
                updateArtworkPalette()
                prewarmNextArtwork()
                forwardOutputClock()
                publishSystemMedia()
            }
        case .error(let issue):
            clearPendingSkip(); pendingOwnedSeek = nil; spatialPreviousConfirmation = nil; errorMessage = issue.message
        case .commandResult(let result):
            acceptForwardedOutputScope(result)
            let outputUpdate = pendingOutputUpdates.remove(result.requestId) != nil
            if let operation = restoredResumeCancellationRequests.removeValue(forKey: result.requestId) {
                if result.error == nil, spotifyStartOperationID == operation {
                    let wantsResume = spotifyStartRestoresSession && !spotifyStartPauseRequested
                    cancelSpotifyStart()
                    if wantsResume, let state = snapshot, state.activeProvider == "spotify",
                       let resume = state.restoredRemote, let item = state.queue.current {
                        startSpotifySelection(item: item, collection: nil, resume: resume, positionMs: state.positionMs)
                    }
                } else {
                    reconcileSpotifyStart()
                }
                return
            }
            if Self.tracePlayback, !outputUpdate, let error = result.error {
                Self.playbackTiming.info("ui_command_rejected request=\(result.requestId, privacy: .public) code=\(error.code, privacy: .public)")
            }
            if outputUpdate && result.error == nil { return }
            if result.error != nil, result.requestId == localLoadingSeekIntent?.request { localLoadingSeekIntent = nil }
            if result.error != nil, result.requestId == pendingOwnedSeek?.request { pendingOwnedSeek = nil }
            if result.error != nil, result.requestId == spatialSeekConfirmation?.seek.request { spatialSeekConfirmation = nil }
            if result.error != nil, result.requestId == spatialPreviousConfirmation?.request { spatialPreviousConfirmation = nil }
            if result.error != nil, result.requestId == pendingLocalPlayback?.request { pendingLocalPlayback = nil }
            if result.error != nil, result.requestId == pendingOwnedPlayback?.request { pendingOwnedPlayback = nil }
            if result.error != nil, result.requestId == restoredPauseOutput?.request {
                restoredPauseOutput = nil
                publishSystemMedia()
            }
            if result.error != nil { rejectHostRelayPause(result.requestId) }
            if result.requestId == spotifyStartTicket, let error = result.error { cancelSpotifyStart(); errorMessage = error.message }
            if result.requestId == pendingSpotifyTransfer?.request, result.error != nil {
                clearSpotifyTransfer(); forwardOutputClock(); publishSystemMedia()
            }
            if result.error != nil,
               result.requestId == pendingSkip?.request || result.requestId == pendingSkip?.localPlayback?.request
                || result.requestId == pendingSkip?.spotifyReturn?.previousRequest { clearPendingSkip() }
            if result.requestId == pendingVolume?.request, result.error != nil { clearPendingVolume() }
            if result.error != nil { clearRejectedQueueModes(result.requestId) }
            let mainQueryID = spotifyMainQueryTickets.removeValue(forKey: result.requestId)
            if let queryID = mainQueryID, let issue = result.error { failSpotifyMainQuery(queryID, issue: issue) }
            let detailQueryID = spotifyDetailTickets[result.requestId]
            if let message = result.error?.message, let queryID = spotifyDetailTickets[result.requestId] { failSpotifyDetail(queryID, message: message) }
            if result.requestId == spotifyEditTicket, let issue = result.error {
                spotifyEditPending = false; spotifyEditTicket = nil; spotifySubmittedEdit = nil; spotifyEditIssue = issue.message
            }
            if !outputUpdate { pendingCommands.remove(result.requestId) }
            lastCommandOutcome = result
            if let error = result.error, detailQueryID == nil, mainQueryID == nil {
                if result.requestId == soundRequest { soundDraft = nil; soundRequest = nil }
                errorMessage = error.message
            }
        }
        reconcileSpotifyStart()
        projectInitialSpotifyOutput()
        publishVisualModulation()
    }
}

extension PlayerStore {
    private func prepareSpotify() {
        guard spotifyAuth == nil else { return }
        spotifyPlayback.restore()
        let epoch = bridgeEpoch
        spotifyAuth = spotifyAuthorizationFactory({ [weak self] status in
            guard let self, bridgeEpoch == epoch else { return }; spotifyAccount = status
        }, { [weak self] token in
            guard let self, bridgeEpoch == epoch else { return }
            acceptSpotifyAccess(token)
        }, { [weak self] in
            guard let self, bridgeEpoch == epoch else { return }
            spotifyCredentialEpoch &+= 1
            clearSpotifyTransfer(); selectedSpotifyDeviceID = nil
            spotifyReadRevoked = true; clearSpotifyReadState(); spotifySaved.removeAll(); spotifyEditPending = false; spotifyEditTicket = nil; spotifyEditSuccess = nil; spotifyEditIssue = nil; spotifyOperationID &+= 1
            LocalArtworkLoader.shared.discardRemote()
            remoteStates.removeValue(forKey: "spotify")
            spotifyState = nil; spotifyPage = nil; spotifyQueryID += 1
            spotifyQuery = RemoteQuery(queryId: 0, kind: .savedTracks, text: "", offset: 0)
            send(.remote(provider: "spotify", action: .disconnect))
        })
        spotifyAuth?.restore(clientID: spotifyClientID)
    }
    /// Receives only the short-lived access credential from the host authorization service.
    /// Never persist/log the credential or reuse read results from its previous generation.
    func acceptSpotifyAccess(_ token: String) {
        spotifyCredentialEpoch &+= 1
        cancelSpotifyStart(); clearSpotifyTransfer(); clearPendingSkip()
        spotifyState?.ownedOutputControl = false
        forwardedOutputQueue = nil
        if spotifyEditPending { spotifyEditIssue = "The connection changed before Spotify confirmed the update. Check your library before retrying." }
        spotifyEditPending = false; spotifyEditTicket = nil; spotifyOperationID &+= 1
        clearSpotifyReadState(); spotifyPage = nil; spotifyReadRevoked = false
        send(.remote(provider: "spotify", action: .credential(token)))
        if spotifyQuery.kind.usesCursor { querySpotify(text: "") }
        else { querySpotify(offset: spotifyQuery.offset) }
        inspectSpotify(.profile)
    }
    func connectSpotify() { spotifyClientID = spotifyClientID.trimmingCharacters(in: .whitespacesAndNewlines); prepareSpotify(); spotifyAuth?.connect(clientID: spotifyClientID) }
    func updateSpotifyPermissions() { spotifyAuth?.updatePermissions() }
    func cancelSpotifyLogin() { spotifyAuth?.cancel() }
    func retrySavedSpotifyLogin() { spotifyAuth?.retrySavedLogin() }
    func disconnectSpotify() { spotifyCredentialEpoch &+= 1; spotifyReadRevoked = true; spotifyPage = nil; clearSpotifyReadState(); spotifySaved.removeAll(); spotifyEditPending = false; spotifyEditTicket = nil; spotifyEditIssue = nil; spotifyEditSuccess = nil; spotifyCreatedPlaylist = nil; cancelSpotifyStart(); clearSpotifyTransfer(); clearPendingSkip(); clearRejectedQueueModes(nil); selectedSpotifyDeviceID = nil; spotifyAuth?.disconnect(); Task { await spotifyPlayback.forgetKey() } }
    func configureSpotifyPlayback(key: String) { spotifyPlayback.configure(key: key) }
    func retrySpotifyPlayback() { spotifyPlayback.retry() }
    func chooseSpotifyRuntime() async {
        guard !isClosing, !isChoosingSpotifyRuntime, !spotifyPlayback.isInstalling, !spotifyPlayback.isForgetting else { return }
        isChoosingSpotifyRuntime = true; defer { isChoosingSpotifyRuntime = false }
        if let source = await fileAccess.chooseSoloistExecutable(), !isClosing {
            spotifyPlayback.installRuntime(from: source)
        }
    }
    var ownedSpotifyDevice: RemoteDevice? {
        if let retained = ownedDeviceContinuation, !canContinueOwnedDevice(retained) { ownedDeviceContinuation = nil }
        guard spotifyPlayback.status.processID != nil, spotifyPlayback.status.authenticated != false else { return nil }
        let matches = (spotifyState?.devices ?? []).filter { $0.name == spotifyPlayback.deviceName && !$0.isRestricted }
        if matches.count == 1 { return matches[0] }
        // The cloud can briefly omit this device while the authenticated local
        // output keeps playing. Preserve only an already observed session's
        // descriptor; a fresh/unplayed selection cannot acquire one this way.
        return ownedDeviceContinuation?.device
    }
    private func canContinueOwnedDevice(_ retained: OwnedDeviceContinuation) -> Bool {
        let now = ProcessInfo.processInfo.systemUptime
        guard !isClosing, !bridgeFailed, !spotifyReadRevoked,
              retained.bridge == bridgeEpoch, retained.runtime == spotifyPlayback.clockGeneration,
              let pid = spotifyPlayback.status.processID, retained.processID == pid, ownedOutputSession == pid,
              spotifyPlayback.status.authenticated == true, spotifyPlayback.status.active == true,
              let snapshot, snapshot.activeProvider == "spotify", snapshot.restoredRemote == nil,
              [.playing, .paused, .loading].contains(snapshot.state), snapshot.queue.current != nil,
              let state = spotifyState, !state.authenticationRequired,
              state.connection == .connected || state.connection == .unavailable,
              let playback = state.playback, [.playing, .paused, .loading].contains(playback.state),
              let current = playback.device, current.id == retained.device.id,
              current.name == retained.device.name, current.isActive, !current.isRestricted,
              spotifyOutputSelectionAllows(retained.device.id),
              let clock = spotifyPlayback.clock, clock.active, clock.isValid(at: now), now - clock.uptime < 0.8,
              state.ownedOutputObserved == true || now - retained.observedUptime < 0.8,
              [.playing, .paused, .loading].contains(clock.state),
              !state.devices.contains(where: { $0.id != retained.device.id && $0.isActive }),
              !state.devices.contains(where: { $0.id == retained.device.id && ($0.isRestricted || $0.name != retained.device.name) }),
              !state.devices.contains(where: { $0.name == retained.device.name && $0.id != retained.device.id })
        else { return false }
        // Media may be between a granular track marker and its projection.
        // This retains device identity only; the provider and host lease still
        // independently validate the new media/epoch before capture or control.
        return true
    }
    private func rememberOwnedSpotifyDevice() {
        if let retained = ownedDeviceContinuation, !canContinueOwnedDevice(retained) { ownedDeviceContinuation = nil }
        if var retained = ownedDeviceContinuation {
            if spotifyState?.ownedOutputObserved == true, let clock = spotifyPlayback.clock,
               SpotifyPlaybackIdentity.matches(clock.mediaId, spotifyState?.playback?.current?.id),
               clock.uptime > retained.observedUptime {
                retained.observedUptime = clock.uptime
                ownedDeviceContinuation = retained
            }
            return
        }
        guard ownedDeviceContinuation == nil,
              let pid = spotifyPlayback.status.processID, ownedOutputSession == pid,
              spotifyPlayback.status.authenticated == true, spotifyPlayback.status.active == true,
              !isClosing, !bridgeFailed, !spotifyReadRevoked,
              let snapshot, snapshot.activeProvider == "spotify", snapshot.state == .playing, snapshot.restoredRemote == nil,
              let state = spotifyState, !state.authenticationRequired, state.ownedOutputObserved == true,
              let playback = state.playback, playback.state == .playing,
              SpotifyPlaybackIdentity.matches(playback.current?.id, snapshot.queue.current?.id),
              let current = playback.device, current.isActive, !current.isRestricted,
              let clock = spotifyPlayback.clock, clock.state == .playing, clock.rate > 0,
              SpotifyPlaybackIdentity.matches(clock.mediaId, snapshot.queue.current?.id)
        else { return }
        let matches = state.devices.filter { $0.name == spotifyPlayback.deviceName && !$0.isRestricted }
        guard matches.count == 1, let device = matches.first, device.id == current.id else { return }
        let retained = OwnedDeviceContinuation(device: device, processID: pid,
            runtime: spotifyPlayback.clockGeneration, bridge: bridgeEpoch, observedUptime: clock.uptime)
        if canContinueOwnedDevice(retained) { ownedDeviceContinuation = retained }
    }
    func pinPlaylist(_ playlist: RemoteCollection?) {
        guard let account = spotifyAccountIdentity else { inspectSpotify(.profile); return }
        guard spotifyPreferences.setFavorite(playlist, for: account) else { return }
        favoritePlaylist = playlist
    }
    private var spotifyAccountIdentity: SpotifyPreferences.Account? {
        guard !spotifyReadRevoked, let page = spotifyDetail(.profile), page.issue == nil,
              page.collections.count == 1, let profile = page.collections.first, profile.entity == .profile,
              SpotifyPreferences.validAccountID(profile.id, stable: false) else { return nil }
        let stableID = profile.accountId.flatMap { SpotifyPreferences.validAccountID($0, stable: true) ? $0 : nil }
        return SpotifyPreferences.Account(profileID: profile.id, stableID: stableID)
    }
    var spotifyProfileID: String? { spotifyAccountIdentity?.profileID }
    func playlistEditAccess(_ playlist: RemoteCollection) -> PlaylistEditAccess {
        if spotifyDetail(.playlistInfo, text: playlist.id)?.issue != nil { return .unknown }
        let playlist = collectionDetails(playlist)
        guard playlist.entity == .playlist, let accountID = spotifyProfileID, let ownerID = playlist.ownerId else { return .unknown }
        if ownerID == accountID { return .owner }
        return playlist.collaborative == true ? .collaborative : .readOnly
    }
    private func knownPlaylist(_ id: String) -> RemoteCollection? {
        if let page = spotifyDetail(.playlistInfo, text: id) { return page.issue == nil ? page.collections.first : nil }
        return spotifyPage?.collections.first { $0.entity == .playlist && $0.id == id } ?? (favoritePlaylist?.id == id ? favoritePlaylist : nil) ?? (spotifyCreatedPlaylist?.id == id ? spotifyCreatedPlaylist : nil)
    }
    func playlistEditAccess(id: String) -> PlaylistEditAccess { knownPlaylist(id).map(playlistEditAccess) ?? .unknown }
    func refreshSpotify() { send(.remote(provider: "spotify", action: .refresh)) }
    func transferSpotify(_ id: String) { beginSpotifyTransfer(to: id, selection: id) }
    func useAuvraPlayback() {
        if let device = ownedSpotifyDevice { beginSpotifyTransfer(to: device.id, selection: nil) }
        else {
            cancelSpotifyStart(); clearSpotifyTransfer(); clearPendingSkip(); clearPendingVolume()
            selectedSpotifyDeviceID = nil; refreshSpotify()
        }
    }
    private func nextSpotifyControlOperation() -> UInt64 {
        spotifyControlSerial &+= 1
        if spotifyControlSerial == 0 { spotifyControlSerial = 1 }
        return spotifyControlSerial
    }
    private func spotifyOutputSelectionAllows(_ id: String) -> Bool {
        (pendingSpotifyTransfer?.deviceID ?? selectedSpotifyDeviceID).map { $0 == id } ?? true
    }
    private func beginSpotifyTransfer(to deviceID: String, selection: String?) {
        guard !isClosing, !bridgeFailed, !isRestarting else { return }
        if let pendingSpotifyTransfer, pendingSpotifyTransfer.deviceID == deviceID, pendingSpotifyTransfer.selection == selection { return }
        ownedPreviousIntent = nil
        let replacing = pendingSpotifyTransfer != nil
        pendingOwnedSeek = nil
        cancelSpotifyStart(); clearSpotifyTransfer(); clearPendingSkip(); clearPendingVolume()
        clearRejectedQueueModes(nil)
        initialSpotifyProjection = false; errorMessage = nil
        if !replacing, let state = spotifyState, state.connection == .connected, !state.authenticationRequired,
           let device = state.playback?.device, device.id == deviceID, device.isActive, !device.isRestricted {
            selectedSpotifyDeviceID = selection
            if snapshot?.activeProvider != "spotify" { send(.remote(provider: "spotify", action: .activate)) }
            forwardOutputClock(); publishSystemMedia(); return
        }
        let operation = nextSpotifyControlOperation()
        pendingSpotifyTransfer = .init(deviceID: deviceID, selection: selection, operation: operation,
                                       request: nextRequestId, observation: spotifyState?.observationId)
        send(.remote(provider: "spotify", action: .control(.transfer(deviceID, operation))))
        publishSystemMedia()
        guard pendingSpotifyTransfer?.operation == operation else { return }
        spotifyTransferTimeout = Task { [weak self] in
            guard let timeout = self?.remoteConfirmationTimeout else { return }
            do { try await Task.sleep(for: timeout) } catch { return }
            guard let self, pendingSpotifyTransfer?.operation == operation else { return }
            clearSpotifyTransfer()
            errorMessage = "Spotify has not confirmed the selected playback device."
            forwardOutputClock(); publishSystemMedia()
        }
    }
    private func clearSpotifyTransfer() {
        pendingSpotifyTransfer = nil; spotifyTransferTimeout?.cancel(); spotifyTransferTimeout = nil
        if errorMessage == Self.spotifyTransferInProgressIssue { errorMessage = nil }
    }
    private func reconcileSpotifyTransfer(_ state: RemoteState) {
        guard state.provider == "spotify", var transfer = pendingSpotifyTransfer else { return }
        if state.authenticationRequired { clearSpotifyTransfer(); return }
        if let result = state.controlResult, result.operationId == transfer.operation {
            if let issue = result.error { clearSpotifyTransfer(); errorMessage = issue.message; return }
            transfer.accepted = true; pendingSpotifyTransfer = transfer
        }
        guard transfer.accepted, state.connection == .connected,
              state.observationId.map({ $0 > (transfer.observation ?? 0) }) ?? (transfer.observation == nil) else { return }
        let target = state.playback?.device ?? (state.playback == nil ? state.devices.first { $0.id == transfer.deviceID } : nil)
        guard let target, target.id == transfer.deviceID, target.isActive, !target.isRestricted else { return }
        selectedSpotifyDeviceID = transfer.selection
        clearSpotifyTransfer()
    }
    private func cancelSpotifyStart() {
        pendingSpotifyStart?.cancel(); pendingSpotifyStart = nil; spotifyStartingItem = nil; spotifyStartingContextURI = nil
        spotifyStartRestoresSession = false; spotifyStartPauseRequested = false
        restoredPauseOutput = nil
        spotifyStartDeviceID = nil; spotifyStartTicket = nil
        spotifyStartOperationID = nil; spotifyStartAccepted = false; spotifyStartSentAt = nil
        spotifyStartDeadline = nil
        spotifyStartCredentialEpoch = nil; spotifyStartAccount = nil
        spotifyStartLocalBaseline = nil
        spotifyStartUncertain = false
        if (timedOutSpotifyItemID != nil || timedOutSpotifyContextURI != nil) && errorMessage == Self.playbackTimeoutIssue { errorMessage = nil }
        timedOutSpotifyItemID = nil; timedOutSpotifyContextURI = nil
        timedOutSpotifyPause = nil
        submittedSpotifyStartScope = nil
        updateArtworkPalette(); prewarmNextArtwork()
    }
    var lightingStatus: String {
        guard reactiveLighting else { return "Music response is off." }
        guard isPlaying else { return "Ambient light while paused." }
        guard analysisVisible else { return "Open Now Playing for music-responsive light." }
        if visualModulation != nil, let frame = audioAnalysis {
            return frame.features.rms > 0.00001 ? "Responding to your music." : "Music is quiet · light is calm."
        }
        switch snapshot?.hostAnalysisStatus {
        case "unsupported_system": return "Music response needs macOS 14.2 or later."
        case "unverified_output": return "Music response is unavailable for this output."
        case "process_unavailable": return "Audio from this playback process isn’t available yet."
        case "tap_unavailable", "device_unavailable", "unsupported_format", "start_unavailable": return "Music response could not start. Turn it off and on to retry."
        default: return "Ambient light · waiting for music audio."
        }
    }
    var spotifySpatialStereoStatus: String {
        guard #available(macOS 14.2, *) else { return "Spotify spatial stereo requires macOS 14.2 or later." }
        guard spotifySpatialStereo else { return "Spotify plays directly through its usual output." }
        guard !bridgeFailed, !isClosing, let snapshot, hostAnalysisLease?.spatialStereo == true,
              ownedAnalysisDevice(for: snapshot, at: ProcessInfo.processInfo.systemUptime, keepingSpatialRelay: true) != nil else {
            return "Play Spotify on the Auvra device to start."
        }
        let fallback: String
        switch snapshot.hostSpatialStatus {
        case "active": return "Auvra is routing Spotify audio. Choose Spatialize Stereo in Control Center → Sound."
        case "paused": return "Spatial audio is ready when Spotify resumes."
        case "waiting_for_route": return "Connect compatible Bluetooth headphones and select them as the Mac audio output."
        case "direct": fallback = String(localized: "Spatial routing was unavailable.")
        case "direct_process": fallback = String(localized: "Auvra could not verify or reach its Spotify playback process.")
        case "direct_tap": fallback = String(localized: "Auvra could not open the Core Audio tap.")
        case "direct_tap_device": fallback = String(localized: "Auvra could not open the tap’s audio capture device.")
        case "direct_tap_format": fallback = String(localized: "The tap returned an unsupported audio format.")
        case "direct_tap_start": fallback = String(localized: "The Core Audio tap could not start.")
        case "direct_renderer": fallback = String(localized: "Auvra could not start the spatial audio renderer.")
        case "direct_renderer_interrupted": fallback = String(localized: "macOS interrupted the spatial audio renderer.")
        case "direct_buffer": fallback = String(localized: "The spatial audio buffer failed.")
        case "direct_input_stalled": fallback = String(localized: "The Core Audio tap stopped delivering audio.")
        case "direct_output_stalled": fallback = String(localized: "The spatial audio renderer stopped advancing.")
        default: return "Preparing spatial audio for Spotify…"
        }
        return fallback + " " + String(localized: "Spotify is playing directly.")
    }
    func setSpotifySpatialStereo(_ enabled: Bool) {
        guard #available(macOS 14.2, *), spotifySpatialStereo != enabled else { return }
        spatialPreviousConfirmation = nil
        spotifySpatialStereo = enabled
        volumeDefaults.set(enabled, forKey: Self.spotifySpatialStereoPreference)
        if enabled, loadsSpotify { SpatialOutputPreflight.start() }
        forwardHostAnalysis()
    }
    func setReactiveLighting(_ enabled: Bool) {
        reactiveLighting = enabled
        UserDefaults.standard.set(enabled, forKey: "AuvraReactiveLighting")
        forwardHostAnalysis()
        publishVisualModulation()
    }
    func setAnalysisVisible(_ visible: Bool) {
        analysisVisible = visible
        forwardHostAnalysis()
    }
    private func suspendHostRelay() {
        guard hostAnalysisLease?.playing == true else { return }
        hostAnalysisLease?.playing = false
        analysisObservation = nil
        send(.hostAnalysis(hostAnalysisLease), outputUpdate: true)
        publishVisualModulation()
    }
    private var keepsPausedSpatialRelay: Bool {
        guard spotifySpatialStereo, ownsPlaybackControl, pendingSkip == nil, !hasPendingOwnedPlayback,
              !hasPendingSpotifySelection, pendingSpotifyTransfer == nil, hostRelayPause == nil,
              let lease = hostAnalysisLease, lease.spatialStereo, !lease.playing,
              lease.processId == spotifyPlayback.status.processID, hostAnalysisRuntime == spotifyPlayback.clockGeneration,
              let snapshot, snapshot.state == .paused, snapshot.hostSpatialStatus == "paused",
              lease.mediaId == snapshot.queue.current?.id, spotifyState?.playback?.state == .paused,
              let clock = spotifyPlayback.clock, clock.state == .paused, clock.rate == 0,
              let device = ownedAnalysisDevice(for: snapshot, at: ProcessInfo.processInfo.systemUptime, keepingSpatialRelay: true)
        else { return false }
        return device.id == lease.deviceId
    }
    private func rejectHostRelayPause(_ request: UInt64?) {
        guard let pause = hostRelayPause, pause.request == request else { return }
        hostRelayPause = nil
        forwardHostAnalysis()
    }
    private func clearHostAnalysis(intent: Bool, sendNativeRevoke: Bool = true) {
        if intent {
            spatialRelativeConfirmation = nil
            fenceHostAnalysisClock()
            hostAnalysisNotBefore = (spatialIntentTime?.uptimeNow() ?? ProcessInfo.processInfo.systemUptime) + 0.35
        }
        guard hostAnalysisLease != nil else { return }
        dropHostAnalysisLease()
        if sendNativeRevoke { send(.hostAnalysis(nil), outputUpdate: true) }
        publishVisualModulation()
    }
    private func retireHostAnalysisForNaturalTransition(visuals: Bool, spatial: Bool, at now: Double) -> Bool {
        guard spatial, !isClosing, !bridgeFailed, !spotifyReadRevoked,
              let lease = hostAnalysisLease, lease.provider == "spotify", lease.spatialStereo,
              lease.visuals == visuals, hostAnalysisRuntime == spotifyPlayback.clockGeneration,
              lease.processId == spotifyPlayback.status.processID, ownedOutputSession == lease.processId,
              spotifyPlayback.status.authenticated == true, spotifyPlayback.status.active == true,
              snapshot?.activeProvider == "spotify", snapshot?.restoredRemote == nil,
              let state = spotifyState, state.provider == "spotify", !state.authenticationRequired,
              state.ownedOutputObserved == true,
              state.connection == .connected || (state.connection == .unavailable && state.ownedOutputControl == true),
              let device = ownedSpotifyDevice, device.id == lease.deviceId,
              state.playback?.device?.id == device.id, state.playback?.device?.isActive == true,
              state.playback?.device?.isRestricted == false, spotifyOutputSelectionAllows(device.id),
              let clock = spotifyPlayback.clock, clock.active, clock.state == .loading, clock.rate == 0,
              clock.isValid(at: now), now - clock.uptime < 0.8,
              let media = clock.mediaId, !SpotifyPlaybackIdentity.matches(media, lease.mediaId),
              pendingSkip == nil, pendingOwnedSeek == nil, pendingLocalPlayback == nil,
              localLoadingSeekIntent == nil, !hasPendingOwnedPlayback, !hasPendingSpotifySelection,
              pendingSpotifyTransfer == nil, pendingShuffle == nil, pendingRepeat == nil,
              spatialSeekConfirmation == nil, spatialPreviousConfirmation == nil, spatialNextOrigin == nil,
              hostRelayPause == nil else { return false }
        if state.connection == .unavailable {
            // Only the active local transport may hand off during a Web outage.
            // An outgoing Loading frame can already have muted the old lease.
            guard spotifyPlayback.status.controlSupported, spatialRelativeConfirmation == nil,
                  let previous = hostAnalysisClock, previous.active, previous.isValid(at: now),
                  now - previous.uptime < 0.8, previous.uptime <= clock.uptime,
                  previous.sequence < clock.sequence, SpotifyPlaybackIdentity.matches(previous.mediaId, lease.mediaId),
                  (previous.state == .playing && previous.rate > 0 || previous.state == .loading && previous.rate == 0),
                  let originEpoch = previous.queueEpoch, originEpoch > 0,
                  let successorEpoch = clock.queueEpoch, successorEpoch > originEpoch else { return false }
        }
        // A track marker can precede its projected snapshot. Retire only this
        // old generation; the coordinator exclusively decides whether its
        // already-proven natural successor may keep zero-only preroll.
        // This command cannot grant a lease or preserve unrelated capture.
        forwardOutputClock()
        dropHostAnalysisLease()
        send(.retireHostAnalysis(generation: lease.generation), outputUpdate: true)
        if Self.tracePlayback {
            Self.playbackTiming.info("spatial_lease_retired generation=\(lease.generation, privacy: .public) sequence=\(clock.sequence, privacy: .public)")
        }
        publishVisualModulation()
        return true
    }
    private func fenceHostAnalysisClock() {
        hostAnalysisFence = spotifyPlayback.clock?.sequence ?? 0
        hostAnalysisFenceRuntime = spotifyPlayback.clockGeneration
    }
    private func dropHostAnalysisLease() {
        hostAnalysisLease = nil; hostAnalysisClock = nil; hostAnalysisSubmittedSequence = 0
        hostRelayPause = nil
        analysisObservation = nil
    }
    private func reconcileHostAnalysisRevocation(_ revoked: UInt64) {
        hostAnalysisGeneration = max(hostAnalysisGeneration, revoked)
        guard let lease = hostAnalysisLease, lease.generation <= revoked else { return }
        // The coordinator already dropped this lease. Wait for a new clock
        // before proposing another, and leave pending transport fences intact.
        fenceHostAnalysisClock()
        dropHostAnalysisLease()
    }
    private func ownedAnalysisDevice(for snapshot: PlaybackSnapshot, at now: Double, keepingSpatialRelay: Bool = false) -> RemoteDevice? {
        let states: [PlaybackState] = keepingSpatialRelay ? [.playing, .paused, .loading] : [.playing]
        guard states.contains(snapshot.state), snapshot.restoredRemote == nil,
              hasOwnedOutput(snapshot, allowingBuffering: keepingSpatialRelay),
              let state = spotifyState, !state.authenticationRequired, state.ownedOutputObserved == true,
              state.connection == .connected || state.connection == .unavailable,
              let playback = state.playback, states.contains(playback.state),
              let device = playback.device, device.isActive, !device.isRestricted,
              let clock = spotifyPlayback.clock, clock.active, states.contains(clock.state),
              clock.isValid(at: now), now - clock.uptime < 0.8,
              SpotifyPlaybackIdentity.matches(clock.mediaId, snapshot.queue.current?.id) else { return nil }
        return device
    }
    private func confirmedSpatialSeek(at now: Double, duringCooldown: Bool) -> (request: UInt64, sequence: UInt64)? {
        guard let proof = spatialSeekConfirmation else { return nil }
        guard duringCooldown, proof.processID == spotifyPlayback.status.processID,
              let clock = matchingOwnedSeekClock(proof.seek),
              (clock.metadata?.contextUri ?? spotifyState?.playback?.contextUri) == proof.contextURI else {
            spatialSeekConfirmation = nil
            return nil
        }
        guard proof.accepted, let dispatch = proof.dispatch, clock.sequence > dispatch.sequence,
              clock.state == .playing, clock.rate > 0, let sourceTimestamp = clock.sourcePositionTimestampMs,
              sourceTimestamp >= dispatch.wallMs,
              sourceTimestamp <= Date().timeIntervalSince1970 * 1000 + 75,
              clock.uptime >= dispatch.uptime,
              now - clock.uptime <= 0.35 else { return nil }
        // A stale pre-seek anchor can receive a new host sequence. Require a fresh
        // source timestamp, the requested position and a jump from natural playback.
        let target = Double(proof.seek.positionMs)
        let observed = Double(clock.positionMs)
        let advanced = (clock.uptime - dispatch.uptime) * clock.rate * 1000
        let uninterrupted = Double(proof.original.positionMs) +
            max(0, clock.uptime - proof.original.uptime) * proof.original.rate * 1000
        guard observed >= target - 120, observed <= target + advanced + 120,
              abs(observed - uninterrupted) >= 500 else { return nil }
        return (proof.seek.request, clock.sequence)
    }
    private func spatialPreviousOrigin() -> SpatialPreviousConfirmation? {
        let now = ProcessInfo.processInfo.systemUptime
        guard spotifySpatialStereo, ownsPlaybackControl, !hasPendingOwnedPlayback, !hasPendingSpotifySelection,
              pendingSpotifyTransfer == nil, hostRelayPause == nil, pendingShuffle == nil, pendingRepeat == nil,
              let snapshot, snapshot.state == .paused, spotifyState?.playback?.state == .paused,
              let clock = spotifyPlayback.clock, clock.state == .paused, clock.rate == 0,
              let epoch = clock.queueEpoch, epoch > 0, let context = clock.metadata?.contextUri,
              let pid = spotifyPlayback.status.processID,
              let device = ownedAnalysisDevice(for: snapshot, at: now, keepingSpatialRelay: true),
              let media = snapshot.queue.current?.id else {
            if Self.tracePlayback { Self.playbackTiming.info("spatial_previous_origin eligible=false") }
            return nil
        }
        if Self.tracePlayback {
            Self.playbackTiming.info("spatial_previous_origin eligible=true retained=\(self.keepsPausedSpatialRelay, privacy: .public)")
        }
        // A rewind may already have retired the renderer. The paused owned output
        // supplies the scope, but only confirmed successor playback can open a tap.
        return SpatialPreviousConfirmation(runtime: spotifyPlayback.clockGeneration, bridge: bridgeEpoch,
            originSequence: clock.sequence, originQueueEpoch: epoch, processID: pid,
            deviceID: device.id, originID: media, contextURI: context)
    }
    private func traceSpatialRelative(_ phase: String, reason: String, proof: SpatialRelativeConfirmation?,
                                      command: RemoteOutputCommand? = nil, mask: UInt64 = 0) {
        guard Self.tracePlayback else { return }
        if let command, command.control.kind != .next && command.control.kind != .previous && command.control.kind != .previousFromStart { return }
        let now = ProcessInfo.processInfo.systemUptime
        let clock = spotifyPlayback.clock
        let frame = spotifyState?.playback
        let queueMatches = clock.map { spotifyPlayback.queue?.matches($0) == true } == true
        let queueFresh = spotifyPlayback.queue.map { $0.isFresh(at: now) && now - $0.uptime <= 0.8 } == true
        let remaining: Int64 = proof.map { proof -> Int64 in
            let duration = relativeProofNow.duration(to: proof.deadline).components
            return max(0, duration.seconds * 1000 + duration.attoseconds / 1_000_000_000_000_000)
        } ?? 0
        let contextMatches = proof.map { clock?.metadata?.contextUri == $0.contextURI } == true
        let originMatches = proof.map { SpotifyPlaybackIdentity.matches(clock?.mediaId, $0.originID) } == true
        let targetMatches = proof?.dispatch.map { SpotifyPlaybackIdentity.matches(clock?.mediaId, $0.target) } == true
        Self.playbackTiming.info("spatial_relative phase=\(phase, privacy: .public) reason=\(reason, privacy: .public) request=\(command?.requestId ?? proof?.request ?? 0, privacy: .public) chain_request=\(proof?.request ?? 0, privacy: .public) wire=\(command?.id ?? proof?.dispatch?.id ?? 0, privacy: .public) kind=\(command?.control.kind.rawValue ?? proof?.kind.rawValue ?? "none", privacy: .public) continued=\(proof?.continued == true, privacy: .public) accepted=\(proof?.accepted == true, privacy: .public) current_sequence=\(clock?.sequence ?? 0, privacy: .public) command_sequence=\(command?.sequence ?? proof?.dispatch?.sequence ?? proof?.originSequence ?? 0, privacy: .public) current_epoch=\(clock?.queueEpoch ?? 0, privacy: .public) command_epoch=\(command?.observedQueueEpoch ?? command?.requiredQueueEpoch ?? proof?.dispatch?.epoch ?? proof?.originEpoch ?? 0, privacy: .public) required_epoch=\(command?.requiredQueueEpoch != nil, privacy: .public) observed_baseline=\(command?.observedQueueEpoch != nil, privacy: .public) full=\(clock?.actions != nil && clock?.metadata != nil, privacy: .public) context_known=\(clock?.metadata?.contextUri != nil, privacy: .public) context_match=\(contextMatches, privacy: .public) origin_match=\(originMatches, privacy: .public) target_match=\(targetMatches, privacy: .public) queue_match=\(queueMatches, privacy: .public) queue_fresh=\(queueFresh, privacy: .public) control=\(self.spotifyState?.ownedOutputControl == true, privacy: .public) observed=\(self.spotifyState?.ownedOutputObserved == true, privacy: .public) clock_state=\(clock?.state.rawValue ?? "none", privacy: .public) frame_state=\(frame?.state.rawValue ?? "none", privacy: .public) snapshot_state=\(self.snapshot?.state.rawValue ?? "none", privacy: .public) remaining_ms=\(remaining, privacy: .public) mask=\(mask, privacy: .public)")
        if phase == "scope_cancelled" || phase == "expired" || phase == "input_unbound" || reason == "scope_gate" {
            Self.playbackTiming.info("spatial_relative_scope request=\(proof?.request ?? 0, privacy: .public) latest=\(proof.map { self.latestPositionRequest == $0.request } == true, privacy: .public) bridge=\(proof.map { self.bridgeEpoch == $0.bridge } == true, privacy: .public) runtime=\(proof.map { self.spotifyPlayback.clockGeneration == $0.runtime } == true, privacy: .public) process=\(proof.map { self.spotifyPlayback.status.processID == $0.processID } == true, privacy: .public) credentials=\(proof.map { self.spotifyCredentialEpoch == $0.credentials } == true, privacy: .public) account_known=\(self.spotifyAccountIdentity != nil, privacy: .public) account_match=\(proof.map { self.spotifyAccountIdentity?.key == $0.account } == true, privacy: .public) closing=\(self.isClosing, privacy: .public) failed=\(self.bridgeFailed, privacy: .public) restarting=\(self.isRestarting, privacy: .public) revoked=\(self.spotifyReadRevoked, privacy: .public) selection=\(self.hasPendingSpotifySelection, privacy: .public) transfer=\(self.pendingSpotifyTransfer != nil, privacy: .public) pause=\(self.hostRelayPause != nil, privacy: .public) authenticated=\(self.spotifyPlayback.status.authenticated == true, privacy: .public) active=\(clock?.active == true, privacy: .public) fresh=\(clock.map { $0.isValid(at: now) && now - $0.uptime <= 0.8 } == true, privacy: .public) device_match=\(proof.map { frame?.device?.id == $0.deviceID } == true, privacy: .public) device_active=\(frame?.device?.isActive == true, privacy: .public) device_restricted=\(frame?.device?.isRestricted == true, privacy: .public)")
        }
    }
    private func traceSpatialRelativeWait(_ proof: SpatialRelativeConfirmation, at now: Double) {
        guard Self.tracePlayback else { return }
        let dispatch = proof.dispatch
        let clock = spotifyPlayback.clock
        let frame = spotifyState?.playback
        // Missing gates, bits 0...16: continuation, ACK, dispatch, full context,
        // moving, fresh, dispatch uptime, sequence, epoch, start position,
        // clock target, snapshot Playing/target, observed/control, frame scope/target.
        let checks = [proof.continued, proof.accepted, dispatch != nil,
            clock?.actions != nil && clock?.metadata?.contextUri == proof.contextURI,
            clock?.state == .playing && (clock?.rate ?? 0) > 0,
            clock.map { now - $0.uptime <= 0.35 } == true,
            dispatch.map { d in clock.map { $0.uptime >= d.uptime } ?? false } == true,
            dispatch.map { d in clock.map { $0.sequence > d.sequence } ?? false } == true,
            dispatch.map { d in clock?.queueEpoch.map { $0 > d.epoch } == true } == true,
            clock.map { $0.positionMs <= 1_000 } == true,
            dispatch.map { SpotifyPlaybackIdentity.matches(clock?.mediaId, $0.target) } == true,
            snapshot?.state == .playing,
            dispatch.map { SpotifyPlaybackIdentity.matches(snapshot?.queue.current?.id, $0.target) } == true,
            spotifyState?.ownedOutputObserved == true, spotifyState?.ownedOutputControl == true,
            frame?.state == .playing && frame?.contextUri == proof.contextURI,
            dispatch.map { SpotifyPlaybackIdentity.matches(frame?.current?.id, $0.target) } == true]
        let mask = checks.enumerated().reduce(UInt64(0)) { $1.element ? $0 : $0 | (UInt64(1) << $1.offset) }
        guard spatialRelativeConfirmation?.diagnosticMask != mask else { return }
        spatialRelativeConfirmation?.diagnosticMask = mask
        traceSpatialRelative("waiting", reason: "confirmation_gate", proof: proof, mask: mask)
    }
    private var relativeProofNow: ContinuousClock.Instant { spatialIntentTime?.continuousNow() ?? .now }
    private func spatialRelativeScope(_ proof: SpatialRelativeConfirmation, at now: Double) -> Bool {
        guard relativeProofNow < proof.deadline, proof.bridge == bridgeEpoch,
              proof.runtime == spotifyPlayback.clockGeneration, proof.credentials == spotifyCredentialEpoch,
              proof.account == spotifyAccountIdentity?.key, proof.processID == spotifyPlayback.status.processID,
              !isClosing, !bridgeFailed, !isRestarting, !spotifyReadRevoked, spotifySpatialStereo,
              snapshot?.activeProvider == "spotify", snapshot?.restoredRemote == nil,
              !hasPendingSpotifySelection, pendingSpotifyTransfer == nil, hostRelayPause == nil,
              spotifyPlayback.status.authenticated == true, spotifyPlayback.status.controlSupported,
              spotifyState?.authenticationRequired == false,
              ownedSpotifyDevice?.id == proof.deviceID, spotifyOutputSelectionAllows(proof.deviceID),
              spotifyState?.devices.contains(where: { $0.isActive && $0.id != proof.deviceID }) != true,
              let frame = spotifyState?.playback, let device = frame.device,
              device.id == proof.deviceID, device.isActive, !device.isRestricted,
              let clock = spotifyPlayback.clock, clock.active, clock.isValid(at: now), now - clock.uptime <= 0.8,
              clock.sequence >= proof.originSequence, clock.queueEpoch.map({ $0 >= proof.originEpoch }) != false,
              clock.metadata?.contextUri == nil || clock.metadata?.contextUri == proof.contextURI,
              clock.state == .loading || clock.state == .playing,
              frame.state == .loading || frame.state == .playing else { return false }
        return true
    }
    private func spatialRelativeOrigin(for command: PlayerCommand) -> SpatialRelativeConfirmation? {
        let kind: RemoteOutputCommand.Control.Kind
        switch command {
        case .next: kind = .next
        case .skipPrevious: kind = .previous
        default: return nil
        }
        let now = ProcessInfo.processInfo.systemUptime
        if var proof = spatialRelativeConfirmation, latestPositionRequest == proof.request,
           spatialRelativeScope(proof, at: now), let clock = spotifyPlayback.clock {
            let moving = pendingSkip.map { $0.provider == "spotify" && $0.request == proof.request } == true
                && ownsPlaybackControl && clock.actions != nil && clock.state == .playing && clock.rate > 0
                && clock.metadata?.contextUri == proof.contextURI && clock.queueEpoch.map { $0 >= proof.originEpoch } == true
            // The selected occurrence can reconcile before its incomplete
            // Loading marker. Preserve only the already dispatched exact target,
            // without replaying the old command or authorizing any capture.
            let loading = clock.state == .loading && clock.metadata != nil
                && proof.dispatch.map { dispatch in
                    SpotifyPlaybackIdentity.matches(clock.mediaId, dispatch.target)
                        && clock.sequence > dispatch.sequence && clock.queueEpoch.map { $0 > dispatch.epoch } == true
                        && clock.uptime >= dispatch.uptime
                } == true
                && (spotifyState?.playback?.contextUri == nil || spotifyState?.playback?.contextUri == proof.contextURI)
            if moving || loading {
                if loading { traceSpatialRelative("loading_continuation", reason: "dispatched_target", proof: proof) }
                proof.kind = kind; proof.continued = true; proof.dispatch = nil; proof.accepted = false; proof.diagnosticMask = nil
                return proof
            }
        }
        guard kind == .next, pendingSkip == nil, ownsPlaybackControl, let account = spotifyAccountIdentity?.key,
              let lease = hostAnalysisLease, lease.spatialStereo, lease.playing,
              hostAnalysisRuntime == spotifyPlayback.clockGeneration,
              let clock = ownedSeekClock, clock.state == .playing, clock.rate > 0,
              now - clock.uptime <= 0.8, clock.actions != nil,
              let epoch = clock.queueEpoch, epoch > 0, let context = clock.metadata?.contextUri,
              lease.processId == spotifyPlayback.status.processID,
              lease.mediaId == clock.mediaId, lease.deviceId == ownedSpotifyDevice?.id else {
            traceSpatialRelative("input_unbound", reason: spatialRelativeConfirmation == nil ? "seed_gate" : "continuation_gate", proof: spatialRelativeConfirmation)
            return nil
        }
        return SpatialRelativeConfirmation(bridge: bridgeEpoch, runtime: spotifyPlayback.clockGeneration,
            credentials: spotifyCredentialEpoch, account: account, processID: lease.processId,
            deviceID: lease.deviceId, originID: lease.mediaId, contextURI: context,
            originSequence: clock.sequence, originEpoch: epoch, deadline: relativeProofNow + .milliseconds(1250))
    }
    private func observeSpatialRelativeDispatch(_ command: RemoteOutputCommand) {
        guard var proof = spatialRelativeConfirmation, command.requestId == proof.request else {
            traceSpatialRelative("dispatch_unbound", reason: spatialRelativeConfirmation == nil ? "no_chain" : "other_request", proof: spatialRelativeConfirmation, command: command)
            return
        }
        let now = ProcessInfo.processInfo.systemUptime
        let kindMatches = proof.kind == .next ? command.control.kind == .next
            : command.control.kind == .previous || command.control.kind == .previousFromStart
        guard kindMatches, latestPositionRequest == proof.request, spatialRelativeScope(proof, at: now),
              spotifyState?.ownedOutputControl == true,
              command.generation == proof.runtime, command.deviceId == proof.deviceID,
              command.sequence >= proof.originSequence, let clock = spotifyPlayback.clock,
              clock.actions != nil || command.observedQueueEpoch.map({ $0 > 0 && clock.queueEpoch == $0 }) == true,
              clock.metadata?.contextUri == proof.contextURI, clock.sequence >= command.sequence,
              SpotifyPlaybackIdentity.matches(clock.mediaId, command.mediaId) else {
            traceSpatialRelative("dispatch_unbound", reason: "scope_gate", proof: proof, command: command)
            spatialRelativeConfirmation = nil; return
        }
        // The provider pins the observed dispatch occurrence without adding a
        // native permission. A newer incomplete heartbeat may preserve that
        // correlation only; actual capture still requires full Playing + ACK.
        // Legacy ordinary commands still require an exact full sequence.
        let ordinary = command.control.kind == .previous || command.control.kind == .next
        let epoch = command.observedQueueEpoch ?? command.requiredQueueEpoch
            ?? (ordinary && clock.sequence == command.sequence ? clock.queueEpoch : nil)
        guard let epoch, epoch > 0, epoch >= proof.originEpoch, clock.queueEpoch == epoch,
              command.requiredQueueEpoch.map({ $0 == epoch }) != false else {
            traceSpatialRelative("dispatch_unbound", reason: "epoch_gate", proof: proof, command: command)
            spatialRelativeConfirmation = nil; return
        }
        let target: String?
        if let expected = command.expectedPreviousMediaId, proof.kind == .previous {
            target = expected
        } else if let queue = spotifyPlayback.queue, queue.matches(clock), queue.isFresh(at: now),
                  now - queue.uptime <= 0.8 {
            let entries = proof.kind == .next ? queue.upcoming : queue.previous
            let head = entries.first?.mediaId
            target = head.flatMap { candidate in
                entries.filter { SpotifyPlaybackIdentity.matches($0.mediaId, candidate) }.count == 1 ? candidate : nil
            }
        } else { target = nil }
        guard let target, !SpotifyPlaybackIdentity.matches(target, command.mediaId),
              !SpotifyPlaybackIdentity.matches(target, proof.originID) else {
            traceSpatialRelative("dispatch_unbound", reason: target == nil ? "target_missing_or_ambiguous" : "target_same_origin", proof: proof, command: command)
            spatialRelativeConfirmation = nil; return
        }
        proof.dispatch = (command.id, command.sequence, epoch, now, target)
        spatialRelativeConfirmation = proof
        traceSpatialRelative("dispatch_bound", reason: clock.actions == nil ? "pinned_partial_origin" : "exact_target", proof: proof, command: command)
    }
    private func confirmedSpatialRelative(at now: Double) -> Bool {
        guard let proof = spatialRelativeConfirmation else { return false }
        guard latestPositionRequest == proof.request, spatialRelativeScope(proof, at: now) else {
            traceSpatialRelative(relativeProofNow >= proof.deadline ? "expired" : "scope_cancelled", reason: "scope_gate", proof: proof)
            spatialRelativeConfirmation = nil; return false
        }
        guard proof.continued, proof.accepted, let dispatch = proof.dispatch,
              let clock = spotifyPlayback.clock, clock.actions != nil,
              clock.metadata?.contextUri == proof.contextURI,
              clock.state == .playing, clock.rate > 0, now - clock.uptime <= 0.35, clock.uptime >= dispatch.uptime,
              clock.sequence > dispatch.sequence, clock.queueEpoch.map({ $0 > dispatch.epoch }) == true,
              clock.positionMs <= 1_000, SpotifyPlaybackIdentity.matches(clock.mediaId, dispatch.target),
              let snapshot, snapshot.state == .playing, SpotifyPlaybackIdentity.matches(snapshot.queue.current?.id, dispatch.target),
              spotifyState?.ownedOutputObserved == true, spotifyState?.ownedOutputControl == true, let frame = spotifyState?.playback,
              frame.state == .playing, frame.contextUri == proof.contextURI,
              SpotifyPlaybackIdentity.matches(frame.current?.id, dispatch.target) else {
            traceSpatialRelativeWait(proof, at: now)
            return false
        }
        traceSpatialRelative("confirmed", reason: "latest_actual_target", proof: proof)
        return true
    }
    private func observeSpatialPreviousDispatch(_ command: RemoteOutputCommand) {
        guard let proof = spatialPreviousConfirmation, command.requestId == proof.request,
              pendingSkip?.request == proof.request, command.generation == proof.runtime,
              spotifyPlayback.clockGeneration == proof.runtime, command.deviceId == proof.deviceID,
              command.requiredQueueEpoch == proof.originQueueEpoch, SpotifyPlaybackIdentity.matches(command.mediaId, proof.originID),
              spotifyPlayback.status.processID == proof.processID, proof.dispatch == nil,
              let expected = command.expectedPreviousMediaId, !SpotifyPlaybackIdentity.matches(expected, proof.originID) else {
            if Self.tracePlayback {
                Self.playbackTiming.info("spatial_previous_dispatch id=\(command.id, privacy: .public) bound=false has_origin=\(self.spatialPreviousConfirmation != nil, privacy: .public) has_target=\(command.expectedPreviousMediaId != nil, privacy: .public)")
            }
            return
        }
        // The provider pins the Previous target to this occurrence before sending the command.
        spatialPreviousConfirmation?.dispatch = (command.id, command.sequence, ProcessInfo.processInfo.systemUptime, expected)
        if Self.tracePlayback {
            Self.playbackTiming.info("spatial_previous_dispatch id=\(command.id, privacy: .public) bound=true")
        }
    }
    private func confirmedSpatialPrevious(at now: Double, duringCooldown: Bool) -> (request: UInt64, sequence: UInt64)? {
        guard let proof = spatialPreviousConfirmation else { return nil }
        let differentDevice = spotifyState?.playback?.device.map { $0.isActive && $0.id != proof.deviceID } ?? false
        guard duringCooldown, proof.bridge == bridgeEpoch, !isClosing, !bridgeFailed, !spotifyReadRevoked,
              spotifyPlayback.clockGeneration == proof.runtime, spotifyPlayback.status.processID == proof.processID,
              spotifyPlayback.status.authenticated == true, spotifyState?.authenticationRequired == false,
              snapshot?.activeProvider == "spotify", !differentDevice,
              spotifyOutputSelectionAllows(proof.deviceID), ownedSpotifyDevice?.id == proof.deviceID else {
            spatialPreviousConfirmation = nil
            return nil
        }
        guard proof.accepted, let dispatch = proof.dispatch, let clock = spotifyPlayback.clock,
              clock.isValid(at: now), now - clock.uptime <= 0.35, clock.uptime >= dispatch.uptime,
              clock.sequence > max(dispatch.sequence, proof.originSequence), clock.active, clock.actions != nil,
              clock.state == .playing, clock.rate > 0, let epoch = clock.queueEpoch, epoch > proof.originQueueEpoch else { return nil }
        guard SpotifyPlaybackIdentity.matches(clock.mediaId, dispatch.predecessor), clock.positionMs <= 1_000,
              clock.metadata?.contextUri == proof.contextURI else {
            if Self.tracePlayback {
                Self.playbackTiming.info("spatial_previous_clock id=\(dispatch.id, privacy: .public) target=\(SpotifyPlaybackIdentity.matches(clock.mediaId, dispatch.predecessor), privacy: .public) context=\(clock.metadata?.contextUri == proof.contextURI, privacy: .public) position_ms=\(clock.positionMs, privacy: .public)")
            }
            spatialPreviousConfirmation = nil
            return nil
        }
        guard let snapshot, snapshot.activeProvider == "spotify", snapshot.state == .playing,
              SpotifyPlaybackIdentity.matches(snapshot.queue.current?.id, dispatch.predecessor),
              spotifyState?.ownedOutputObserved == true, let frame = spotifyState?.playback,
              frame.state == .playing, frame.contextUri == proof.contextURI,
              frame.device?.id == proof.deviceID, frame.device?.isActive == true, frame.device?.isRestricted == false,
              frame.current?.id == snapshot.queue.current?.id else { return nil }
        return (proof.request, clock.sequence)
    }
    private func spatialClockChangeOrigin(lease: HostAnalysisLease, previous: SoloistClock,
                                          clock: SoloistClock, now: Double, error: Double) -> UInt64? {
        guard lease.playing, lease.spatialStereo, ownedOutputSession == lease.processId,
              snapshot?.hostSpatialStatus == "active", spotifyPlayback.status.authenticated == true,
              spotifyState?.ownedOutputObserved == true, hostRelayPause == nil,
              pendingSkip == nil, pendingOwnedSeek == nil, pendingLocalPlayback == nil,
              !hasPendingOwnedPlayback, !hasPendingSpotifySelection, pendingSpotifyTransfer == nil,
              pendingShuffle == nil, pendingRepeat == nil, localLoadingSeekIntent == nil,
              spatialSeekConfirmation == nil, spatialPreviousConfirmation == nil, spatialRelativeConfirmation == nil,
              previous.active, clock.active, previous.state == .playing, clock.state == .playing,
              previous.rate > 0, previous.rate == clock.rate, abs(error) > 250,
              previous.isValid(at: now), now - previous.uptime <= 0.8,
              now - clock.uptime <= 0.35, clock.uptime >= previous.uptime,
              clock.sequence > previous.sequence, previous.actions != nil, clock.actions != nil,
              previous.mediaId == clock.mediaId, SpotifyPlaybackIdentity.matches(clock.mediaId, lease.mediaId),
              let epoch = previous.queueEpoch, epoch > 0, clock.queueEpoch == epoch,
              let context = previous.metadata?.contextUri, clock.metadata?.contextUri == context,
              spotifyState?.playback?.contextUri == context,
              let sourceTimestamp = clock.sourcePositionTimestampMs, sourceTimestamp.isFinite,
              previous.sourcePositionTimestampMs.map({ sourceTimestamp > $0 }) != false else { return nil }
        let age = Date().timeIntervalSince1970 * 1000 - sourceTimestamp
        guard (-75...350).contains(age) else { return nil }
        return previous.sequence
    }
    private func forwardHostAnalysis() {
        let now = ProcessInfo.processInfo.systemUptime
        let duringCooldown = (spatialIntentTime?.uptimeNow() ?? now) < hostAnalysisNotBefore
        let confirmedSeek = confirmedSpatialSeek(at: now, duringCooldown: duringCooldown)
        let confirmedPrevious = confirmedSpatialPrevious(at: now, duringCooldown: duringCooldown)
        let confirmedRelative = confirmedSpatialRelative(at: now)
        let runtime = spotifyPlayback.clockGeneration
        let visuals = analysisVisible && reactiveLighting
        let spatial: Bool
        if #available(macOS 14.2, *) { spatial = spotifySpatialStereo } else { spatial = false }
        guard visuals || spatial, !isClosing, !bridgeFailed, !isRestarting,
              let snapshot, let pid = spotifyPlayback.status.processID,
              let media = current?.id, let clock = spotifyPlayback.clock,
              !duringCooldown || ((confirmedSeek != nil || confirmedPrevious != nil || confirmedRelative) && spatial),
              runtime != hostAnalysisFenceRuntime || clock.sequence > hostAnalysisFence else {
            clearHostAnalysis(intent: false); return
        }
        if clock.state == .paused || hostRelayPause.map({ now >= $0.deadline }) == true { hostRelayPause = nil }
        // Keep queued/intermediate occurrences out of the tap until the latest
        // dispatched relative intent is actually playing. This proof never
        // extends the original chain deadline or grants native transport.
        if spatialRelativeConfirmation?.continued == true, !confirmedRelative {
            clearHostAnalysis(intent: false); return
        }
        if !confirmedRelative, let origin = spatialNextOrigin, now < origin.deadline, SpotifyPlaybackIdentity.matches(clock.mediaId, origin.mediaId) {
            return
        }
        if retireHostAnalysisForNaturalTransition(visuals: visuals, spatial: spatial, at: now) { return }
        let sameOutput = hostAnalysisLease.map {
            $0.processId == pid && $0.mediaId == media && $0.deviceId == spotifyState?.playback?.device?.id
                && $0.spatialStereo == spatial && hostAnalysisRuntime == runtime
        } ?? false
        guard let device = ownedAnalysisDevice(for: snapshot, at: now, keepingSpatialRelay: sameOutput && spatial) else {
            clearHostAnalysis(intent: false); return
        }
        let clockIsPlaying = clock.state == .playing && (!spatial || clock.rate > 0)
        var discontinuity = false
        var clockChange: (generation: UInt64, originSequence: UInt64)?
        if let previous = hostAnalysisClock, !spatial || (previous.state == .playing && previous.rate > 0 && clockIsPlaying) {
            // Soloist may publish zero position/rate while paused or buffering.
            // A suspended relay discards its audio and needs no position continuity.
            let elapsed = max(0, clock.uptime - previous.uptime)
            let expected = Double(previous.positionMs) + elapsed * previous.rate * 1000
            let error = Double(clock.positionMs) - expected
            let rateChanged = previous.rate != clock.rate
            discontinuity = elapsed > 0.8 || abs(error) > 250 || rateChanged
            if spatial, sameOutput, discontinuity, let lease = hostAnalysisLease,
               let sequence = spatialClockChangeOrigin(lease: lease, previous: previous, clock: clock, now: now, error: error) {
                clockChange = (lease.generation, sequence)
            }
            if Self.tracePlayback, spatial, sameOutput, discontinuity {
                Self.playbackTiming.info("spatial_clock_reset elapsed_ms=\(Int((elapsed * 1000).rounded()), privacy: .public) delta_ms=\(Int(error.rounded()), privacy: .public) rate_changed=\(rateChanged, privacy: .public)")
            }
        }
        let purposeChanged = hostAnalysisLease?.visuals != visuals
        if !sameOutput || discontinuity {
            let pause = hostRelayPause
            clearHostAnalysis(intent: false, sendNativeRevoke: clockChange == nil)
            hostRelayPause = pause
            guard snapshot.state == .playing, spotifyState?.playback?.state == .playing, clockIsPlaying,
                  hostRelayPause == nil, hostAnalysisGeneration < UInt64.max else {
                if clockChange != nil { send(.hostAnalysis(nil), outputUpdate: true) }
                return
            }
            hostAnalysisGeneration += 1
            hostAnalysisLease = HostAnalysisLease(generation: hostAnalysisGeneration, processId: pid, provider: "spotify", deviceId: device.id, mediaId: media, spatialStereo: spatial, visuals: visuals)
            hostAnalysisRuntime = runtime
            if spatial {
                Self.playbackTiming.info("spatial_lease_granted generation=\(self.hostAnalysisGeneration, privacy: .public)")
            }
        }
        let playing = clockIsPlaying && hostRelayPause == nil
        let transportChanged = hostAnalysisLease?.playing != playing
        if Self.tracePlayback, spatial, transportChanged {
            Self.playbackTiming.info("spatial_lease_transport generation=\(self.hostAnalysisGeneration, privacy: .public) playing=\(playing, privacy: .public) sequence=\(clock.sequence, privacy: .public)")
        }
        hostAnalysisClock = clock
        if confirmedRelative { hostAnalysisNotBefore = 0 }
        if confirmedRelative || (spatialRelativeConfirmation?.continued == false && hostAnalysisLease?.mediaId != spatialRelativeConfirmation?.originID) {
            spatialRelativeConfirmation = nil
        }
        guard purposeChanged || transportChanged || hostAnalysisSubmittedSequence != clock.sequence else { return }
        hostAnalysisLease?.visuals = visuals
        hostAnalysisLease?.playing = playing
        if (purposeChanged && !visuals) || (transportChanged && !playing) {
            analysisObservation = nil
            publishVisualModulation()
        }
        hostAnalysisSubmittedSequence = clock.sequence
        if let confirmedSeek, let lease = hostAnalysisLease, lease.spatialStereo, lease.playing {
            hostAnalysisNotBefore = 0
            spatialSeekConfirmation = nil
            if Self.tracePlayback {
                Self.playbackTiming.info("spatial_seek_cue request=\(confirmedSeek.request, privacy: .public) sequence=\(confirmedSeek.sequence, privacy: .public)")
            }
            forwardOutputClock()
            send(.hostAnalysisAfterSeek(lease, requestId: confirmedSeek.request, sequence: confirmedSeek.sequence), outputUpdate: true)
        } else if let clockChange, let lease = hostAnalysisLease, lease.playing {
            forwardOutputClock()
            send(.hostAnalysisAfterClockChange(lease, retiredGeneration: clockChange.generation,
                originSequence: clockChange.originSequence, sequence: clock.sequence), outputUpdate: true)
            if Self.tracePlayback {
                Self.playbackTiming.info("spatial_clock_reanchor submitted=true old_generation=\(clockChange.generation, privacy: .public) new_generation=\(lease.generation, privacy: .public) sequence=\(clock.sequence, privacy: .public)")
            }
        } else {
            if let confirmedPrevious, hostAnalysisLease?.spatialStereo == true, hostAnalysisLease?.playing == true {
                hostAnalysisNotBefore = 0
                spatialPreviousConfirmation = nil
                if Self.tracePlayback {
                    Self.playbackTiming.info("spatial_previous_cue request=\(confirmedPrevious.request, privacy: .public) sequence=\(confirmedPrevious.sequence, privacy: .public)")
                }
                forwardOutputClock()
            }
            send(.hostAnalysis(hostAnalysisLease), outputUpdate: true)
        }
    }
    private func forwardOutputClock() {
        guard !isClosing else { return }
        reconcileOwnedSeek()
        guard !bridgeFailed, !spotifyReadRevoked, let state = spotifyState,
              !state.authenticationRequired, state.connection == .connected || state.connection == .unavailable,
              spotifyPlayback.status.authenticated == true, let device = ownedSpotifyDevice,
              spotifyOutputSelectionAllows(device.id),
              let clock = spotifyPlayback.clock, clock.isValid(at: ProcessInfo.processInfo.systemUptime) else {
            if forwardedOutputClock != nil {
                if Self.tracePlayback {
                    let clock = spotifyPlayback.clock
                    let valid = clock?.isValid(at: ProcessInfo.processInfo.systemUptime) == true
                    Self.playbackTiming.info("output_clock_removed known_device=\(self.ownedSpotifyDevice != nil, privacy: .public) authenticated=\(self.spotifyPlayback.status.authenticated == true, privacy: .public) valid=\(valid, privacy: .public) active=\(clock?.active == true, privacy: .public) paused=\(clock?.state == .paused, privacy: .public)")
                }
                forwardedOutputClock = nil
                send(.remote(provider: "spotify", action: .outputClock(nil)), outputUpdate: true)
            }
            forwardedOutputQueue = nil
            return
        }
        let generation = spotifyPlayback.clockGeneration
        if forwardedOutputClock?.generation != generation || forwardedOutputClock?.sequence != clock.sequence || forwardedOutputClock?.device != device.id {
            guard let observation = spotifyPlayback.observation(device: device.id) else { return }
            if Self.tracePlayback, forwardedOutputClock?.state != clock.state || forwardedOutputClock?.active != clock.active {
                Self.playbackTiming.info("output_clock state=\(clock.state.rawValue, privacy: .public) active=\(clock.active, privacy: .public)")
            }
            forwardedOutputClock = (generation, clock.sequence, device.id, clock.state, clock.active)
            if let processID = spotifyPlayback.status.processID {
                let now = ProcessInfo.processInfo.systemUptime
                pendingOutputScopes = pendingOutputScopes.filter {
                    $0.value.bridge == bridgeEpoch && $0.value.generation == generation && now - $0.value.uptime <= 2
                }
                if pendingOutputScopes.count >= 8, let oldest = pendingOutputScopes.keys.min() {
                    pendingOutputScopes.removeValue(forKey: oldest)
                }
                pendingOutputScopes[nextRequestId] = AcceptedOutputScope(bridge: bridgeEpoch,
                    generation: generation, sequence: clock.sequence, processID: processID,
                    device: device.id, uptime: clock.uptime)
            }
            send(.remote(provider: "spotify", action: .outputClock(observation)), outputUpdate: true)
        }
        let now = ProcessInfo.processInfo.systemUptime
        guard let queue = spotifyPlayback.queue, queue.matches(clock), queue.isFresh(at: now),
              forwardedOutputQueue?.generation != generation || forwardedOutputQueue?.sequence != queue.sequence || forwardedOutputQueue?.device != device.id else { return }
        forwardedOutputQueue = (generation, queue.sequence, device.id)
        let observation = RemoteOutputQueue(generation: generation, epoch: queue.epoch, sequence: queue.sequence,
                                            deviceId: device.id, mediaId: queue.mediaId, previous: queue.previous, upcoming: queue.upcoming,
                                            autoplayReady: queue.autoplayReady == true && now - queue.uptime <= 1.2 ? true : nil)
        send(.remote(provider: "spotify", action: .outputQueue(observation)), outputUpdate: true)
    }
    private func acceptForwardedOutputScope(_ result: CommandOutcome) {
        guard let scope = pendingOutputScopes.removeValue(forKey: result.requestId), result.error == nil,
              !isClosing, !isRestarting, !bridgeFailed, scope.bridge == bridgeEpoch,
              scope.generation == spotifyPlayback.clockGeneration,
              ProcessInfo.processInfo.systemUptime - scope.uptime <= 2 else { return }
        if let previous = acceptedOutputScope, previous.generation == scope.generation,
           previous.sequence >= scope.sequence { return }
        acceptedOutputScope = scope
    }
    private func ownedOutputInterrupted(_ interruption: SoloistRuntime.ChildInterruption) {
        let scope = acceptedOutputScope
        pendingOutputScopes.removeAll(); acceptedOutputScope = nil
        let scopeAge = scope.map { ProcessInfo.processInfo.systemUptime - $0.uptime }
        if Self.tracePlayback {
            Self.playbackTiming.info("owned_interruption_scope present=\(scope != nil, privacy: .public) generation=\(interruption.generation, privacy: .public) sequence=\(scope?.sequence ?? 0, privacy: .public) age_ms=\((scopeAge ?? -1) * 1000, privacy: .public) same_bridge=\(scope?.bridge == self.bridgeEpoch, privacy: .public) same_runtime=\(scope?.generation == interruption.generation, privacy: .public) same_process=\(scope?.processID == interruption.processID, privacy: .public) closing=\(self.isClosing, privacy: .public) restarting=\(self.isRestarting, privacy: .public) bridge_failed=\(self.bridgeFailed, privacy: .public)")
        }
        guard !isClosing, !isRestarting, !bridgeFailed, let scope,
              scope.bridge == bridgeEpoch, scope.generation == interruption.generation,
              scope.processID == interruption.processID,
              let scopeAge, scopeAge <= 2 else { return }
        // Rust owns the canonical selection and its paused restore. This is
        // only a stop witness for an already accepted clock, never metadata.
        send(.ownedOutputInterrupted(provider: "spotify", generation: scope.generation,
            sequence: scope.sequence, deviceId: scope.device), outputUpdate: true)
    }
    private func cancelOwnedOutputRecovery() {
        pendingOutputScopes.removeAll(); acceptedOutputScope = nil
        guard !isClosing, !bridgeFailed else { return }
        send(.cancelOwnedOutputRecovery(provider: "spotify"), outputUpdate: true)
    }
    private var spotifyStartTarget: (item: String?, context: String?) {
        (spotifyStartingItemID ?? timedOutSpotifyItemID, spotifyStartingContextURI ?? timedOutSpotifyContextURI)
    }
    private var isSpotifyContextItemStart: Bool {
        let target = spotifyStartTarget
        return target.item != nil && target.context != nil
    }
    private func matchesSpotifyStart(_ frame: RemotePlayback) -> Bool {
        let target = spotifyStartTarget
        guard target.item != nil || target.context != nil else { return false }
        return (target.item.map { SpotifyPlaybackIdentity.matches($0, frame.current?.id) } ?? true)
            && (target.context.map { $0 == frame.contextUri } ?? true)
    }
    private var canApplyTimedOutSpotifyPause: Bool {
        guard let pause = timedOutSpotifyPause, ContinuousClock.now < pause.expires,
              !isClosing, !bridgeFailed, !spotifyReadRevoked, spotifyState?.authenticationRequired == false,
              pause.operation == spotifyStartOperationID, pause.device == spotifyStartDeviceID,
              pause.bridge == bridgeEpoch, pause.runtime == spotifyPlayback.clockGeneration,
              pause.account == spotifyAccountIdentity?.key else { return false }
        return true
    }
    private func reconcileSpotifyStart() {
        if timedOutSpotifyPause != nil && !canApplyTimedOutSpotifyPause {
            timedOutSpotifyPause = nil
        }
        guard spotifyStartHasConfirmedFrame else { return }
        let claimedActivation = claimSubmittedSpotifyActivation()
        if let operation = spotifyStartOperationID {
            Self.playbackTiming.info("selection_confirmed operation=\(operation, privacy: .public)")
        }
        let latePause = canApplyTimedOutSpotifyPause
        let pauseTarget = (spotifyStartPauseRequested || latePause) && !spotifyReadRevoked ? spotifyState?.playback?.device : nil
        let restoredPauseOperation = spotifyStartRestoresSession || (latePause && timedOutSpotifyPause?.restoresSession == true) ? spotifyStartOperationID : nil
        let restoredPauseMedia = restoredPauseOperation == nil ? nil : current?.id
        cancelSpotifyStart()
        if claimedActivation && pauseTarget == nil { publishSystemMedia() }
        guard let pauseTarget, pauseTarget.isActive, !pauseTarget.isRestricted,
              spotifyOutputSelectionAllows(pauseTarget.id) else { return }
        // Pause follows the confirmed selection. Sending it while Start is still
        // pending could cancel that write or pause the previous track instead.
        let request: UInt64
        if let operation = restoredPauseOperation {
            let owned = ownsPlaybackControl
            request = send(.pauseConfirmedRestoredResume(operationId: operation, deviceId: pauseTarget.id))
            if owned {
                pendingOwnedPlayback = (request, false, ProcessInfo.processInfo.systemUptime + 2, isPlaying)
            }
        } else {
            // The confirmed row can precede the normal capability projection.
            // Address the selected device even if its generic Pause flag changes.
            let owned = ownsPlaybackControl
            request = send(.remote(provider: "spotify", action: .control(.pauseOnDevice(pauseTarget.id))))
            if owned {
                pendingOwnedPlayback = (request, false, ProcessInfo.processInfo.systemUptime + 2, isPlaying)
            }
        }
        if let media = restoredPauseMedia, pauseTarget.id == ownedSpotifyDevice?.id {
            restoredPauseOutput = (request, pauseTarget.id, media, spotifyPlayback.clockGeneration,
                                   ProcessInfo.processInfo.systemUptime + 2)
        }
    }
    private var spotifyStartHasConfirmedFrame: Bool {
        guard let sent = spotifyStartSentAt, let deviceID = spotifyStartDeviceID,
              snapshot?.activeProvider == "spotify", let state = spotifyState, let frame = state.playback,
              snapshot?.state == .playing, snapshot?.restoredRemote == nil, frame.state == .playing,
              frame.current?.id == current?.id, frame.device?.id == deviceID,
              matchesSpotifyStart(frame) else { return false }
        if isSpotifyContextItemStart {
            guard frame.device?.isActive == true, frame.device?.isRestricted == false,
                  let observation = state.observationId, observation > spotifyStartContextObservation else { return false }
        }
        if let baseline = spotifyStartLocalBaseline {
            guard state.ownedOutputObserved == true, let observation = state.observationId, observation > baseline.observation,
                  spotifyPlayback.clockGeneration == baseline.generation,
                  spotifyStartAccepted || (spotifyStartUncertain && !baseline.wasPlayingTarget) else { return false }
        } else {
            guard spotifyStartAccepted, state.connection == .connected, snapshot?.providerConnection == .connected else { return false }
        }
        let rate = frame.positionRate ?? 1
        let elapsed = max(0, ProcessInfo.processInfo.systemUptime - sent) * 1000 * rate
        let position = Double(frame.positionMs)
        return position >= max(0, Double(spotifyStartPosition) - 2000) && position <= Double(spotifyStartPosition) + elapsed + 4000
    }
    private func claimSubmittedSpotifyActivation() -> Bool {
        guard unclaimedSpotifyActivation != nil,
              let operation = spotifyStartOperationID, validateSpotifyStartAccount(operation),
              let deadline = spotifyStartDeadline, ContinuousClock.now < deadline,
              let scope = submittedSpotifyStartScope, scope.bridge == bridgeEpoch,
              scope.operation == spotifyStartOperationID, scope.runtime == spotifyPlayback.clockGeneration,
              scope.processID == spotifyPlayback.status.processID,
              scope.credentials == spotifyCredentialEpoch,
              scope.account == spotifyAccountIdentity?.key,
              let sent = spotifyStartSentAt, let device = ownedSpotifyDevice,
              spotifyStartDeviceID == device.id,
              spotifyPlayback.status.processID != nil, spotifyPlayback.status.authenticated == true,
              let clock = spotifyPlayback.clock, clock.active, clock.state == .playing, clock.rate > 0,
              clock.uptime >= sent, clock.isValid(at: ProcessInfo.processInfo.systemUptime),
              ProcessInfo.processInfo.systemUptime - clock.uptime < 0.8,
              SpotifyPlaybackIdentity.matches(clock.mediaId, current?.id) else { return false }
        unclaimedSpotifyActivation = nil
        return true
    }
    private func projectInitialSpotifyOutput() {
        // Let the coordinator resolve account-bound restore before adopting a live output.
        guard initialSpotifyProjection, !isClosing, !bridgeFailed, !isLoading,
              snapshot?.session.ready == true, spotifyProfileID != nil, snapshot?.restoredRemote == nil,
              let state = spotifyState, state.connection == .connected,
              let device = ownedSpotifyDevice, state.playback?.device?.id == device.id, state.playback?.current != nil else { return }
        if state.playback?.state == .playing, spotifyPlayback.status.clockSupported {
            guard let clock = spotifyPlayback.clock, clock.active, clock.state == .playing, SpotifyPlaybackIdentity.matches(clock.mediaId, state.playback?.current?.id) else { return }
        }
        initialSpotifyProjection = false
        send(.remote(provider: "spotify", action: .adoptOutput))
    }
    func startSpotifyTrack(_ item: MediaItem) { startSpotifySelection(item: item, collection: nil) }
    func playSpotifyCatalogItem(_ item: MediaItem, in collection: RemoteCollection?, queryId: UInt64, index: Int) {
        guard item.capabilities.play else { return }
        guard let collection, collection.entity == .album || collection.entity == .playlist,
              let reference = SpotifyContentReference(item.id), reference.kind == .track, item.id == reference.uri else {
            startSpotifyTrack(item)
            return
        }
        guard !spotifyReadRevoked, !bridgeFailed, !isClosing, SpotifyContentReference(collection.uri) != nil,
              spotifyQuery.queryId == queryId, spotifyQuery.kind == collection.childrenKind, spotifyQuery.text == collection.id,
              let page = spotifyPage, page.provider == "spotify", page.queryId == queryId, page.detail != true,
              page.issue == nil || page.hasOmittedResults, page.items.indices.contains(index),
              page.items[index].id == item.id, page.items[index].capabilities.play else { return }
        guard let positions = page.itemPositions, positions.count == page.items.count,
              positions[index] < 100_000 else {
            errorMessage = "Refresh this collection before starting the selected row."
            return
        }
        let row = RemoteContextRow(position: positions[index], queryId: queryId)
        startSpotifySelection(item: spotifyCatalogItem(page.items[index], in: collection), collection: collection, row: row)
    }
    func continueSpotifySpoken(_ item: MediaItem, queryId: UInt64) {
        guard item.capabilities.play, let position = spotifySpokenProgress(for: item, queryId: queryId)?.continuePosition else { return }
        startSpotifySelection(item: item, collection: nil, positionMs: position)
    }
    private func submitSpotifyStart(_ control: RemoteControl, to device: String) {
        guard let operation = spotifyStartOperationID, validateSpotifyStartAccount(operation) else { return }
        spotifyStartSentAt = ProcessInfo.processInfo.systemUptime
        spotifyStartDeviceID = device
        spotifyStartContextObservation = spotifyState?.observationId ?? 0
        submittedSpotifyStartScope = (bridgeEpoch, operation, spotifyPlayback.clockGeneration, spotifyPlayback.status.processID, spotifyCredentialEpoch, spotifyStartAccount)
        if let operation = spotifyStartOperationID {
            Self.playbackTiming.info("selection_submitted operation=\(operation, privacy: .public)")
        }
        spotifyStartTicket = send(.remote(provider: "spotify", action: .control(control)))
    }
    private func validateSpotifyStartAccount(_ operation: UInt64) -> Bool {
        guard spotifyStartOperationID == operation, spotifyStartCredentialEpoch == spotifyCredentialEpoch else { return false }
        let current = spotifyAccountIdentity?.key
        if let bound = spotifyStartAccount { return current == bound }
        if let current {
            // A first profile response resolves an unknown identity under the
            // same credentials; it is not an account switch. Bind only once.
            spotifyStartAccount = current
            if var submitted = submittedSpotifyStartScope,
               submitted.operation == operation, submitted.bridge == bridgeEpoch,
               submitted.credentials == spotifyCredentialEpoch, submitted.account == nil {
                submitted.account = current; submittedSpotifyStartScope = submitted
            }
        }
        return true
    }
    private func startSpotifySelection(item: MediaItem?, collection: RemoteCollection?, row: RemoteContextRow? = nil, resume: RestoredRemote? = nil, positionMs: UInt64? = nil) {
        guard !isRestarting, !isClosing, !bridgeFailed else { return }
        guard pendingSpotifyTransfer == nil else { errorMessage = Self.spotifyTransferInProgressIssue; return }
        cancelSpotifyStart(); clearPendingSkip(); initialSpotifyProjection = false; trackTransitionDirection = 0
        guard item?.capabilities.play == true || collection?.canPlayContext == true else { return }
        guard selectedSpotifyDeviceID != nil || ownedSpotifyDevice != nil || spotifyPlayback.status.phase == .ready || spotifyPlayback.status.phase == .starting else {
            errorMessage = Self.playbackSetupIssue; return
        }
        ownedPreviousIntent = nil
        let deadline = ContinuousClock.now.advanced(by: remoteConfirmationTimeout)
        spotifyStartDeadline = deadline
        pendingOwnedSeek = nil
        errorMessage = nil; spotifyStartRestoresSession = resume != nil
        spotifyStartingItem = item; spotifyStartingContextURI = collection?.uri
        updateArtworkPalette(); prewarmNextArtwork()
        let operation = nextSpotifyControlOperation(); spotifyStartOperationID = operation; spotifyStartPosition = positionMs ?? 0
        spotifyStartCredentialEpoch = spotifyCredentialEpoch; spotifyStartAccount = spotifyAccountIdentity?.key
        Self.playbackTiming.info("selection_requested operation=\(operation, privacy: .public)")
        func control(_ device: String) -> RemoteControl {
            if let resume, let item { return .resumeOnDevice(item, device, positionMs ?? 0, resume.contextUri, operation) }
            if let positionMs, let item { return .startSpokenOnDevice(item, device, positionMs, operation) }
            if let collection, let item, let row { return .startContextItem(item, collection.uri, device, operation, row) }
            if let collection { return .startContext(collection.uri, device, operation) }
            return .startOnDevice(item!, device, operation)
        }
        let epoch = bridgeEpoch
        let credentials = spotifyCredentialEpoch
        let runtime = spotifyPlayback.clockGeneration
        let initialPID = spotifyPlayback.status.processID
        let selectedDevice = selectedSpotifyDeviceID
        let initialDevice = selectedDevice ?? ownedSpotifyDevice?.id
        let activation = initialDevice == nil && resume == nil ? send(.remote(provider: "spotify", action: .activate)) : nil
        spotifyStartTicket = activation
        let initiallyNeedsBootstrap = selectedDevice == nil && spotifyPlayback.status.controlSupported
            && spotifyPlayback.status.authenticated == true && spotifyPlayback.status.active != true
        if let initialDevice, !initiallyNeedsBootstrap { submitSpotifyStart(control(initialDevice), to: initialDevice) }
        else if initialDevice == nil { refreshSpotify() }
        pendingSpotifyStart = Task { [weak self] in
            guard let self else { return }
            var boundPID = initialPID
            var attemptedBootstrap = false
            @MainActor func scopeIsCurrent() -> Bool {
                guard !Task.isCancelled, bridgeEpoch == epoch, spotifyStartOperationID == operation,
                      !isClosing, !isRestarting, !bridgeFailed, !spotifyReadRevoked,
                      spotifyState?.authenticationRequired != true,
                      spotifyCredentialEpoch == credentials, validateSpotifyStartAccount(operation),
                      selectedSpotifyDeviceID == selectedDevice, pendingSpotifyTransfer == nil else { return false }
                if selectedDevice == nil {
                    guard spotifyPlayback.clockGeneration == runtime else { return false }
                    if let boundPID { guard spotifyPlayback.status.processID == boundPID else { return false } }
                    else if let pid = spotifyPlayback.status.processID { boundPID = pid }
                }
                return true
            }
            @MainActor func timedOut() {
                if spotifyStartSentAt == nil {
                    errorMessage = "Playback has not reconnected yet. Check Playback on this Mac in Settings → Account."
                    return
                }
                timedOutSpotifyItemID = item?.id; timedOutSpotifyContextURI = collection?.uri
                if spotifyStartPauseRequested,
                   spotifyStartOperationID == operation, let device = spotifyStartDeviceID,
                   let scope = submittedSpotifyStartScope {
                    // A submitted write may finish late. Keep its existing,
                    // bounded Pause grace without starting any new write.
                    timedOutSpotifyPause = .init(operation: operation, device: device, bridge: epoch,
                        runtime: scope.runtime, account: scope.account, restoresSession: spotifyStartRestoresSession,
                        expires: .now.advanced(by: remotePauseConfirmationGrace))
                }
                errorMessage = Self.playbackTimeoutIssue
            }
            defer {
                if !Task.isCancelled && bridgeEpoch == epoch && spotifyStartOperationID == operation {
                    pendingSpotifyStart = nil; spotifyStartingItem = nil; spotifyStartingContextURI = nil; spotifyStartTicket = nil
                    spotifyStartRestoresSession = false; spotifyStartPauseRequested = false
                    if timedOutSpotifyItemID == nil && timedOutSpotifyContextURI == nil {
                        submittedSpotifyStartScope = nil; spotifyStartDeadline = nil
                        spotifyStartCredentialEpoch = nil; spotifyStartAccount = nil
                        spotifyStartDeviceID = nil; spotifyStartOperationID = nil; spotifyStartSentAt = nil
                    }
                    updateArtworkPalette(); prewarmNextArtwork()
                }
            }
            var lastRefresh = initialDevice == nil ? ContinuousClock.now : .now.advanced(by: .seconds(-2))
            while spotifyStartSentAt == nil {
                guard scopeIsCurrent() else { return }
                guard ContinuousClock.now < deadline else { timedOut(); return }
                let needsBootstrap = selectedDevice == nil && spotifyPlayback.status.controlSupported
                    && spotifyPlayback.status.authenticated == true && spotifyPlayback.status.active != true
                if needsBootstrap && !attemptedBootstrap {
                    if spotifyPlayback.canActivateForSelection, let pid = spotifyPlayback.status.processID {
                        attemptedBootstrap = true
                        unclaimedSpotifyActivation = .init(bridge: epoch, runtime: runtime, operation: operation, processID: pid)
                        ownedOutputSession = nil
                        publishSystemMedia()
                        let playback = spotifyPlayback
                        let activationTask = Task { @MainActor in await playback.activateForSelection() }
                        let deadlineTask = Task {
                            do { try await Task.sleep(until: deadline, clock: .continuous) } catch { return }
                            activationTask.cancel()
                        }
                        let accepted = await withTaskCancellationHandler {
                            await activationTask.value
                        } onCancel: { activationTask.cancel() }
                        deadlineTask.cancel()
                        guard scopeIsCurrent() else { return }
                        guard ContinuousClock.now < deadline else { timedOut(); return }
                        guard accepted else { errorMessage = "The playback engine could not activate. Try again."; return }
                        // Let a known device's Start reach the worker before
                        // scheduling another playback read. If the device has
                        // disappeared, the discovery loop below refreshes it.
                    }
                }
                if (!needsBootstrap || attemptedBootstrap),
                   (activation == nil || !pendingCommands.contains(activation!)),
                   snapshot?.activeProvider == "spotify", let device = selectedDevice ?? ownedSpotifyDevice?.id {
                    guard scopeIsCurrent(), ContinuousClock.now < deadline else { return }
                    submitSpotifyStart(control(device), to: device)
                    break
                }
                if lastRefresh.duration(to: .now) >= .seconds(2) { refreshSpotify(); lastRefresh = .now }
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
            }
            // Bootstrap, discovery and confirmation share the original budget.
            while true {
                guard scopeIsCurrent() else { return }
                guard ContinuousClock.now < deadline else { timedOut(); return }
                if spotifyStartHasConfirmedFrame { reconcileSpotifyStart(); return }
                if let state = spotifyState, let issue = state.issue, !state.transportAvailable { errorMessage = issue.message; return }
                do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
            }
        }
    }
    func canEnqueueSpotifyTrack(_ item: MediaItem) -> Bool {
        guard item.capabilities.play, let state = spotifyState, state.transportAvailable,
              let frame = spotifyState?.playback, frame.device?.isActive == true, frame.device?.isRestricted == false else { return false }
        let parts = item.id.split(separator: ":")
        return parts.count == 3 && parts[0] == "spotify" && (parts[1] == "track" || (parts[1] == "episode" && state.connection == .connected)) && parts[2].utf8.count == 22 && parts[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) && (frame.transport.queueAdd ?? true)
    }
    func enqueueSpotifyTrack(_ item: MediaItem) {
        guard canEnqueueSpotifyTrack(item) else { return }
        send(.remote(provider: "spotify", action: .control(.enqueue(item))))
    }
    func spotifyVisibility(_ visible: Bool) { send(.remote(provider: "spotify", action: .visibility(visible))) }
    var isLoadingSpotifyPage: Bool { spotifyQuery.queryId != 0 && spotifyPage?.queryId != spotifyQuery.queryId }
    func presentSpotifyCatalog(kind: RemoteCatalogKind, text: String) {
        if kind.usesCursor, let page = spotifyCursorPages[kind]?.last {
            querySpotify(kind: kind, text: page.value, offset: page.offset)
        } else {
            querySpotify(kind: kind, text: text)
        }
    }
    func querySpotify(kind: RemoteCatalogKind? = nil, text: String? = nil, offset: UInt32 = 0) {
        if let kind, kind.isDetail { inspectSpotify(kind, text: text ?? ""); return }
        let kind = kind ?? spotifyQuery.kind
        var cursorOrText = RemoteQuery.boundedText(text ?? spotifyQuery.text)
        var pageOffset = offset
        if kind.usesCursor {
            guard !spotifyReadRevoked else { return }
            if offset == 0 || cursorOrText.isEmpty || (text == nil && kind != spotifyQuery.kind) {
                cursorOrText = ""
                pageOffset = 0
            }
            let page = SpotifyCursorPage(value: cursorOrText, offset: pageOffset)
            if spotifyCursorPages[kind]?.last != page {
                spotifyCursorPages[kind] = page == .first ? [.first] : [.first, page]
            }
        }
        spotifyQueryID += 1
        spotifyQuery = RemoteQuery(queryId: spotifyQueryID, kind: kind, text: cursorOrText, offset: pageOffset)
        guard spotifyMainQueryTickets.count < 64 else {
            failSpotifyMainQuery(spotifyQueryID, issue: PlayerIssue(code: "busy", message: "Wait for the previous requests to finish, then try again.")); return
        }
        send(.remote(provider: "spotify", action: .query(spotifyQuery)))
    }
    private func failSpotifyMainQuery(_ queryID: UInt64, issue: PlayerIssue) {
        guard !spotifyReadRevoked, queryID == spotifyQuery.queryId else { return }
        spotifyPage = RemotePage(detail: false, provider: "spotify", queryId: queryID, offset: spotifyQuery.offset, total: 0, items: [], collections: [], issue: issue)
    }
    private func detailKey(_ kind: RemoteCatalogKind, _ text: String) -> String { kind.rawValue + ":" + text }
    func spotifyDetail(_ kind: RemoteCatalogKind, text: String = "") -> RemotePage? { spotifyDetails[detailKey(kind, text)] }
    func spotifySpokenProgress(for item: MediaItem, queryId: UInt64) -> SpokenProgress? {
        guard !spotifyReadRevoked, !bridgeFailed, !isClosing,
              let reference = SpotifyContentReference(item.id), [.episode, .chapter].contains(reference.kind) else { return nil }
        let page = spotifyQuery.queryId == queryId ? spotifyPage : spotifyDetail(reference.kind, text: reference.id)
        guard let page, page.queryId == queryId, page.provider == "spotify", page.issue == nil || page.hasOmittedResults,
              let original = page.items.first(where: { $0.id == item.id }),
              let progress = page.spokenProgress?.first(where: { $0.mediaId == item.id }) else { return nil }
        let duration = min(original.durationMs, item.durationMs)
        guard duration > 0, progress.positionMs <= duration,
              progress.fullyPlayed || progress.positionMs < duration else { return nil }
        if progress.continuePosition != nil {
            if page.detail == true && isLoadingSpotifyDetail(reference.kind, text: reference.id) { return nil }
            if let snapshot, snapshot.activeProvider == "spotify", snapshot.restoredRemote == nil, snapshot.state == .playing,
               SpotifyPlaybackIdentity.matches(item.id, snapshot.queue.current?.id) { return nil }
            if let observed = spotifySpokenPlayback.first(where: { $0.id == reference.id }) {
                guard let accountKey = spotifyAccountIdentity?.key else { return nil }
                if spotifySpokenPlaybackAccountKey == accountKey {
                    guard queryId > observed.queryID else { return nil }
                    if observed.pausedSeek {
                        // After a paused seek, an older bookmark can be on either side.
                        let low = min(progress.positionMs, observed.positionMs)
                        let high = max(progress.positionMs, observed.positionMs)
                        guard high - low <= 1_000 else { return nil }
                    } else if progress.positionMs < observed.positionMs { return nil }
                }
            }
        }
        return progress
    }
    private func observeSpotifySpokenPlayback(_ state: PlaybackSnapshot) {
        guard state.activeProvider == "spotify", state.restoredRemote == nil,
              let accountKey = spotifyAccountIdentity?.key, let item = state.queue.current,
              let reference = SpotifyContentReference(item.id), [.episode, .chapter].contains(reference.kind),
              item.durationMs > 0, state.positionMs <= item.durationMs else { return }
        let followsObservedPlayback = state.state == .paused && snapshot?.activeProvider == "spotify" && snapshot?.restoredRemote == nil
            && SpotifyPlaybackIdentity.matches(item.id, snapshot?.queue.current?.id)
            && spotifySpokenPlaybackAccountKey == accountKey && spotifySpokenPlayback.contains { $0.id == reference.id }
        let justPaused = followsObservedPlayback && snapshot?.state == .playing
        let pausedSeek = followsObservedPlayback && snapshot?.state == .paused && snapshot?.positionMs != state.positionMs
        guard state.state == .playing || justPaused || pausedSeek else { return }
        if spotifySpokenPlaybackAccountKey != accountKey {
            spotifySpokenPlaybackAccountKey = accountKey
            spotifySpokenPlayback.removeAll()
        }
        // A page requested before this frame can arrive later with the old bookmark.
        spotifySpokenPlayback.removeAll { $0.id == reference.id }
        spotifySpokenPlayback.append((reference.id, spotifyQueryID, state.positionMs, pausedSeek))
        if spotifySpokenPlayback.count > 32 { spotifySpokenPlayback.removeFirst() }
    }
    func spotifyRelatedCollections(for item: MediaItem) -> [RemoteCollection] {
        let parts = item.id.split(separator: ":").map(String.init)
        guard parts.count == 3, parts[0] == "spotify" else { return [] }
        let kind: RemoteCatalogKind
        let allowed: Set<RemoteEntityKind>
        switch parts[1] {
        case "track": kind = .track; allowed = [.album, .artist]
        case "episode": kind = .episode; allowed = [.show]
        case "chapter": kind = .chapter; allowed = [.audiobook]
        default: return []
        }
        guard let page = spotifyDetail(kind, text: parts[2]), page.issue == nil, page.items.first?.id == item.id else { return [] }
        var seen: Set<String> = []
        return Array((page.relatedCollections ?? []).prefix(17).filter {
            allowed.contains($0.entity) && $0.id.utf8.count == 22 && $0.id.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) && seen.insert($0.uri).inserted
        })
    }
    func isLoadingSpotifyDetail(_ kind: RemoteCatalogKind, text: String = "") -> Bool {
        spotifyDetailQueries.values.contains { $0.kind == kind && $0.text == text }
    }
    func spotifyNextArtistAlbumOffset(_ id: String) -> UInt32? {
        guard let page = spotifyDetail(.artistAlbums, text: id), page.offset <= 99_990 else { return nil }
        let next = page.offset + 10
        return UInt64(next) < page.total ? next : nil
    }
    func loadMoreSpotifyArtistAlbums(_ id: String, at offset: UInt32) {
        guard offset == spotifyNextArtistAlbumOffset(id) else { return }
        inspectSpotify(.artistAlbums, text: id, offset: offset)
    }
    func inspectSpotify(_ kind: RemoteCatalogKind, text: String = "", refresh: Bool = false, offset: UInt32 = 0) {
        guard kind.isDetail, !spotifyReadRevoked, !bridgeFailed, !isClosing else { return }
        guard offset == 0 || (kind == .artistAlbums && offset == spotifyNextArtistAlbumOffset(text)) else { return }
        let key = detailKey(kind, text)
        guard !isLoadingSpotifyDetail(kind, text: text), refresh || offset != 0 || spotifyDetails[key] == nil else { return }
        spotifyQueryID &+= 1
        let query = RemoteQuery(queryId: spotifyQueryID, kind: kind, text: String(text.prefix(256)), offset: offset)
        guard spotifyDetailQueries.count < 8 else {
            cacheSpotifyDetail(query, page: RemotePage(detail: true, provider: "spotify", queryId: query.queryId, offset: offset, total: 0, items: [], collections: [], issue: PlayerIssue(code: "rate_limited", message: "Wait for the other details to load, then try again.")))
            return
        }
        if offset != 0 { spotifyDetailOrder.removeAll { $0 == key }; spotifyDetailOrder.append(key) }
        spotifyDetailQueries[query.queryId] = query
        let ticket = send(.remote(provider: "spotify", action: .query(query)))
        if spotifyDetailQueries[query.queryId] != nil { spotifyDetailTickets[ticket] = query.queryId }
    }
    private func cacheSpotifyDetail(_ query: RemoteQuery, page: RemotePage) {
        let key = detailKey(query.kind, query.text)
        let cached: RemotePage
        if query.kind == .artistAlbums {
            let previous = query.offset == 0 ? nil : spotifyDetails[key]
            let canAppend = page.issue == nil || page.issue?.code == "track_unavailable"
            let existing = previous?.collections ?? []
            var seen = Set(existing.map(\.uri))
            let releases = canAppend ? existing + page.collections.filter { seen.insert($0.uri).inserted } : existing
            cached = RemotePage(detail: true, provider: page.provider, queryId: page.queryId,
                                offset: canAppend ? page.offset : previous?.offset ?? 0,
                                total: canAppend ? page.total : previous?.total ?? 0,
                                items: [], collections: releases, issue: page.issue)
        } else { cached = page }
        spotifyDetailOrder.removeAll { $0 == key }; spotifyDetailOrder.append(key); spotifyDetails[key] = cached
        while spotifyDetailOrder.count > 32 {
            let index = spotifyDetailOrder.firstIndex { $0 != detailKey(.profile, "") } ?? 0
            spotifyDetails.removeValue(forKey: spotifyDetailOrder.remove(at: index))
        }
        if query.kind == .profile {
            favoritePlaylist = nil
            if let account = spotifyAccountIdentity, let pin = spotifyPreferences.favorite(for: account) {
                favoritePlaylist = RemoteCollection(id: pin.playlistID, title: pin.title, url: "https://open.spotify.com/playlist/\(pin.playlistID)", artwork: nil, kind: .playlist)
                inspectSpotify(.playlistInfo, text: pin.playlistID)
            }
        } else if query.kind == .playlistInfo, page.issue == nil,
                  let playlist = page.collections.first, playlist.entity == .playlist, playlist.id == favoritePlaylistID, let account = spotifyAccountIdentity {
            favoritePlaylist = playlist
            if spotifyPreferences.favorite(for: account)?.title != String(playlist.title.prefix(160)) { spotifyPreferences.setFavorite(playlist, for: account) }
        }
    }
    private func acceptSpotifyDetail(_ page: RemotePage) {
        guard let query = spotifyDetailQueries.removeValue(forKey: page.queryId) else { return }
        spotifyDetailTickets = spotifyDetailTickets.filter { $0.value != page.queryId }
        let collectionKind: RemoteEntityKind? = switch query.kind {
        case .playlistInfo, .playlistImages: .playlist
        default: RemoteEntityKind(rawValue: query.kind.rawValue)
        }
        let matches: Bool
        if query.kind == .artistAlbums || query.kind == .homeArtistAlbums {
            matches = page.items.isEmpty && page.collections.count <= 10
                && page.collections.allSatisfy { album in
                    album.entity == .album && SpotifyContentReference(album.uri)?.uri == album.uri
                }
        } else if query.kind == .artistTopTracks || query.kind == .artistSelectedTracks {
            matches = page.collections.isEmpty && page.items.count <= 10
                && page.items.allSatisfy { item in
                    guard let reference = SpotifyContentReference(item.id) else { return false }
                    return reference.kind == .track && reference.uri == item.id
                }
        } else if query.text.isEmpty {
            matches = true
        } else if let collectionKind {
            matches = page.items.isEmpty && page.collections.count <= 1 &&
                page.collections.allSatisfy { $0.id == query.text && $0.entity == collectionKind }
        } else {
            matches = page.collections.isEmpty && page.items.count <= 1 &&
                page.items.allSatisfy { $0.id == "spotify:\(query.kind.rawValue):\(query.text)" }
        }
        if matches && page.offset == query.offset { cacheSpotifyDetail(query, page: page) }
        else {
            cacheSpotifyDetail(query, page: RemotePage(detail: true, provider: "spotify", queryId: page.queryId, offset: query.offset, total: 0, items: [], collections: [], issue: PlayerIssue(code: "invalid_response", message: "These details did not match the requested content. Try again.")))
        }
    }
    private func failSpotifyDetail(_ queryID: UInt64, message: String) {
        guard let query = spotifyDetailQueries[queryID] else { return }
        acceptSpotifyDetail(RemotePage(detail: true, provider: "spotify", queryId: queryID, offset: query.offset, total: 0, items: [], collections: [], issue: PlayerIssue(code: "unavailable", message: message)))
    }
    private func observeSpotifySavedPage(_ page: RemotePage) {
        guard page.issue == nil || page.hasOmittedResults else { return }
        let uris: [String] = switch spotifyQuery.kind {
        case .savedTracks, .savedEpisodes: page.items.map(\.id)
        case .savedAlbums, .savedShows, .savedAudiobooks, .followedArtists: page.collections.map(\.uri)
        default: []
        }
        let pendingMembership = spotifySubmittedEdit.flatMap { edit in
            edit.kind == "library" || edit.kind == "check_library" ? edit.uris : nil
        } ?? []
        for uri in uris where spotifySaved[uri] != true
            && (spotifySavedPageFloor[uri].map { page.queryId > $0 } ?? true)
            && !pendingMembership.contains(uri) {
            rememberSpotifySaved(uri, saved: true)
        }
    }
    private func rememberSpotifySaved(_ uri: String, saved: Bool, throughQuery: UInt64? = nil) {
        spotifySaved[uri] = saved; spotifySavedOrder.removeAll { $0 == uri }; spotifySavedOrder.append(uri)
        spotifySavedPageFloor[uri] = saved ? nil : throughQuery
        while spotifySavedOrder.count > 2048 {
            let oldest = spotifySavedOrder.removeFirst()
            spotifySaved.removeValue(forKey: oldest)
            spotifySavedPageFloor.removeValue(forKey: oldest)
        }
    }
    private func clearSpotifyReadState() {
        favoritePlaylist = nil
        spotifyCursorPages.removeAll()
        spotifySubmittedEdit = nil; spotifyCreatedPlaylist = nil; spotifyEditSuccess = nil
        spotifySaved.removeAll(); spotifySavedOrder.removeAll(); spotifySavedPageFloor.removeAll()
        spotifyDetailEpoch &+= 1; spotifyDetails.removeAll(); spotifyDetailOrder.removeAll(); spotifyDetailQueries.removeAll(); spotifyDetailTickets.removeAll()
    }
    func collectionDetails(_ collection: RemoteCollection) -> RemoteCollection {
        let candidate = spotifyDetail(collection.detailKind, text: collection.id)?.collections.first
        let update = candidate?.id == collection.id && candidate?.entity == collection.entity ? candidate : nil
        let cover = collection.entity == .playlist ? spotifyDetail(.playlistImages, text: collection.id) : nil
        let artwork = cover != nil && cover?.issue == nil ? cover?.collections.first?.artwork : update?.artwork ?? collection.artwork
        return RemoteCollection(id: collection.id, title: update?.title ?? collection.title, url: update?.url ?? collection.url, artwork: artwork, isPublic: update?.isPublic ?? collection.isPublic, kind: collection.kind, subtitle: update?.subtitle?.isEmpty == false ? update?.subtitle : collection.subtitle, description: update?.description ?? collection.description, ownerId: update != nil ? update?.ownerId : collection.ownerId, collaborative: update != nil ? update?.collaborative : collection.collaborative)
    }
    func spotifyCatalogItem(_ item: MediaItem, in collection: RemoteCollection?) -> MediaItem {
        // Simplified chapter/episode rows omit their parent. Reuse only the
        // matching visible collection; never issue a request per child.
        guard !spotifyReadRevoked, let collection,
              spotifyQuery.kind == collection.childrenKind, spotifyQuery.text == collection.id,
              let page = spotifyPage, page.queryId == spotifyQuery.queryId, page.issue == nil,
              page.items.contains(where: { $0.id == item.id }) else { return item }
        let child: String
        switch collection.entity {
        case .audiobook: child = "chapter"
        case .show: child = "episode"
        case .album: child = "track"
        default: return item
        }
        guard item.id.hasPrefix("spotify:\(child):") else { return item }
        let parent = collectionDetails(collection)
        let creators = parent.subtitle.flatMap { $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : String($0.prefix(256)) }
        return MediaItem(id: item.id, title: item.title,
                         artists: item.artists.isEmpty ? creators.map { [$0] } ?? [] : item.artists,
                         album: item.album ?? parent.title, durationMs: item.durationMs,
                         artwork: item.artwork ?? parent.artwork, capabilities: item.capabilities, location: item.location)
    }
    private func invalidateSpotifyDetail(_ kind: RemoteCatalogKind, text: String) {
        let key = detailKey(kind, text); spotifyDetails.removeValue(forKey: key); spotifyDetailOrder.removeAll { $0 == key }
        let ids = Set(spotifyDetailQueries.values.filter { $0.kind == kind && $0.text == text }.map(\.queryId))
        spotifyDetailQueries = spotifyDetailQueries.filter { !ids.contains($0.key) }
        spotifyDetailTickets = spotifyDetailTickets.filter { !ids.contains($0.value) }
    }
    func editSpotify(_ edit: RemoteEdit) {
        guard !spotifyEditPending else { return }
        if edit.kind == "create_playlist", !edit.isValidCreatePlaylist {
            spotifyEditSuccess = nil; spotifyEditIssue = "Review the playlist name, description and publishing settings before creating it."
            return
        }
        if edit.kind == "playlist_details", !edit.isValidPlaylistDetailsPatch {
            spotifyEditSuccess = nil; spotifyEditIssue = "This playlist change is empty or invalid. Edit a field and try again."
            return
        }
        if edit.kind == "playlist_move" {
            guard spotifyQuery.kind == .playlistItems, spotifyQuery.text == edit.id,
                  let page = spotifyPage, page.queryId == spotifyQuery.queryId, page.queryId == edit.queryId,
                  page.positionEdits == true, let from = edit.from, let to = edit.to,
                  page.items.indices.contains(where: { page.position(at: $0) == from }),
                  from < 100_000, to <= 100_000, UInt64(to) <= page.total, from != to, UInt64(from) + 1 != UInt64(to) else {
                spotifyEditSuccess = nil; spotifyEditIssue = "Reload this playlist before moving an item."
                return
            }
        }
        if edit.kind.hasPrefix("playlist_"), let id = edit.id {
            let access = knownPlaylist(id).map(playlistEditAccess) ?? .unknown
            let allowed = ["playlist_details", "playlist_cover"].contains(edit.kind) ? access.canEditDetails : access.canEditItems
            guard allowed else {
                spotifyEditSuccess = nil
                spotifyEditIssue = access == .unknown ? "Load your profile and playlist details before editing." : "Your account cannot make this change to this playlist."
                if access == .unknown { inspectSpotify(.profile); inspectSpotify(.playlistInfo, text: id) }
                return
            }
        }
        spotifyOperationID &+= 1; spotifyEditPending = true; spotifyEditIssue = nil; spotifyEditSuccess = nil; spotifyCreatedPlaylist = nil
        spotifySubmittedEdit = edit
        spotifyEditTicket = send(.remote(provider: "spotify", action: .edit(.init(operationId: spotifyOperationID, edit: edit))))
    }
    func checkSpotifyURI(_ uri: String) { editSpotify(.init(kind: "check_library", uris: [uri])) }
    func saveSpotifyURI(_ uri: String, save: Bool) { editSpotify(.init(kind: "library", uris: [uri], save: save)) }
    func playSpotifyCollection(_ collection: RemoteCollection) { startSpotifySelection(item: nil, collection: collection) }
    private var spotifyNextCursor: String? {
        guard !spotifyReadRevoked, spotifyQuery.kind.usesCursor, spotifyQuery.offset <= 99_990,
              let page = spotifyPage, page.queryId == spotifyQuery.queryId,
              page.issue == nil || page.hasOmittedResults, let next = page.nextCursor,
              !next.isEmpty, next == RemoteQuery.boundedText(next),
              spotifyCursorPages[spotifyQuery.kind]?.contains(where: { $0.value == next }) == false else { return nil }
        return next
    }
    var spotifyCanAdvanceCursor: Bool { spotifyNextCursor != nil }
    var spotifyPreviousCursorIsFirst: Bool {
        spotifyQuery.kind.usesCursor && spotifyQuery.offset > 10 && spotifyCursorPages[spotifyQuery.kind]?.count == 2
    }
    func spotifyPageBy(_ direction: Int) {
        if spotifyQuery.kind.usesCursor {
            let kind = spotifyQuery.kind
            guard !spotifyReadRevoked, spotifyPage?.queryId == spotifyQuery.queryId,
                  var pages = spotifyCursorPages[kind] else { return }
            if direction > 0 {
                guard let cursor = spotifyNextCursor else { return }
                // Retain the first page and the most recently visited pages for this route.
                if pages.count == 64 { pages.remove(at: 1) }
                pages.append(SpotifyCursorPage(value: cursor, offset: spotifyQuery.offset + 10))
            } else {
                guard direction < 0, pages.count > 1 else { return }
                pages.removeLast()
            }
            guard let page = pages.last else { return }
            spotifyCursorPages[kind] = pages
            querySpotify(text: page.value, offset: page.offset)
            return
        }
        let next = UInt32(clamping: max(0, Int(spotifyQuery.offset) + direction * 10))
        guard !spotifyQuery.kind.isSearch || next <= 1000 else { return }
        querySpotify(offset: next)
    }
}


extension PlayerStore {
    var isScanningLibrary: Bool { libraryState?.scan != nil }
    var isLoadingLibraryPage: Bool { libraryPage?.queryId != libraryQuery.queryId }
    func restoreLibrary() async {
        let epoch = bridgeEpoch
        do {
            let configuration = try await libraryFiles.restore()
            guard epoch == bridgeEpoch else { return }
            send(.initializeLibrary(directory: configuration.directory, roots: configuration.roots))
            if sessionEnabled {
                let files = try await sessionFiles.restore()
                let roots = await libraryFiles.sessionRoots()
                guard epoch == bridgeEpoch else { return }
                send(.initializeSession(directory: configuration.directory, access: SessionAccess(roots: roots, files: files)))
            }
        } catch { guard epoch == bridgeEpoch else { return }; errorMessage = "The library folders could not be restored: \(error.localizedDescription)" }
    }
    func chooseLibraryFolder(replacing id: String? = nil) async {
        guard !isChoosingFolder else { return }
        isChoosingFolder = true; defer { isChoosingFolder = false }
        if let url = await LibraryFolderChooser.choose() { await addLibraryFolder(url, replacing: id) }
    }
    func addLibraryFolder(_ url: URL, replacing id: String? = nil) async {
        do { let root = try await libraryFiles.add(url, replacing: id); send(.addLibraryRoot(root)); await refreshQueueAccess() }
        catch { errorMessage = "The folder could not be added: \(error.localizedDescription)" }
    }
    func removeLibraryFolder(_ id: String) async {
        do { try await libraryFiles.remove(id); send(.removeLibraryRoot(id)); await refreshQueueAccess() }
        catch { errorMessage = "The folder could not be removed: \(error.localizedDescription)" }
    }
    func scanLibrary(_ id: String? = nil) { send(.scanLibrary(id)) }
    func cancelLibraryScan() { send(.cancelLibraryScan) }
    func restoreQueue() async {
        guard sessionEnabled else { return }
        let epoch = bridgeEpoch
        do {
            let configuration = try await libraryFiles.restore()
            let access = SessionAccess(roots: await libraryFiles.sessionRoots(), files: try await sessionFiles.restore())
            guard epoch == bridgeEpoch else { return }
            if snapshot?.session.ready == true { send(.refreshSessionAccess(access)) }
            else { send(.initializeSession(directory: configuration.directory, access: access)) }
        } catch { guard epoch == bridgeEpoch else { return }; errorMessage = "Queue access could not be restored: \(error.localizedDescription)" }
    }
    private func refreshQueueAccess() async {
        guard sessionEnabled, snapshot?.session.ready == true else { return }
        await restoreQueue()
    }
    func browseLibrary(mode: LibraryMode? = nil, text: String? = nil, rootId: String? = nil, clearRoot: Bool = false) {
        if let mode { libraryQuery.mode = mode; libraryQuery.albumId = nil; libraryAlbum = nil }
        if let text { libraryQuery.text = String(text.prefix(256)) }
        if clearRoot { libraryQuery.rootId = nil } else if let rootId { libraryQuery.rootId = rootId }
        libraryQuery.offset = 0; requestLibraryPage()
    }
    func openLibraryAlbum(_ album: LibraryAlbum) {
        libraryAlbum = album; libraryQuery.albumId = album.id; libraryQuery.mode = .tracks
        libraryQuery.offset = 0; requestLibraryPage()
    }
    func libraryPageBy(_ direction: Int) {
        let offset = max(0, Int(libraryQuery.offset) + direction * Int(libraryQuery.limit))
        libraryQuery.offset = UInt32(clamping: offset); requestLibraryPage()
    }
    func playLibrary(_ track: LibraryTrack) {
        guard track.available else { return }
        send(.playLibrary(query: libraryQuery, trackId: track.item.id))
    }
    func playLibraryAlbum() {
        guard libraryQuery.albumId != nil else { return }; send(.playLibrary(query: libraryQuery, trackId: ""))
    }
    private func requestLibraryPage() {
        guard libraryState?.ready == true else { return }
        libraryQueryId += 1
        libraryQuery = LibraryQuery(queryId: libraryQueryId, mode: libraryQuery.mode, text: libraryQuery.text,
                                    albumId: libraryQuery.albumId, rootId: libraryQuery.rootId,
                                    offset: libraryQuery.offset, limit: 100)
        send(.queryLibrary(libraryQuery))
    }
    private func applyLibraryState(_ state: LibraryState) {
        let changed = libraryState?.revision != state.revision
        libraryState = state
        if let id = libraryQuery.rootId, !state.roots.contains(where: { $0.id == id }) { libraryQuery.rootId = nil }
        if state.ready && (changed || libraryPage == nil) { requestLibraryPage() }
    }
}
