import Foundation
import Observation
import Security
import Darwin
import OSLog

enum SoloistPhase: String, Sendable { case notConfigured, starting, pairing, ready, failed, expired }
struct SoloistStatus: Sendable {
    var active: Bool?
    var clockSupported = false
    var controlSupported = false
    var phase: SoloistPhase = .notConfigured
    var issue: String?
    var processID: Int32?
    var authenticated: Bool?
}

protocol SoloistInstalling: Sendable {
    func install(_ source: URL) async throws
}

/// Stage and verify a user-selected copy before touching the active executable.
/// File IO stays independent of the actor that sends playback commands.
actor SoloistRuntimeInstaller: SoloistInstalling {
    static let supportDirectory = URL.applicationSupportDirectory.appendingPathComponent("Auvra", isDirectory: true)
    static let runtimeDirectory = supportDirectory.appendingPathComponent("PlaybackRuntime", isDirectory: true)
    private static let maximumBytes = 128 * 1024 * 1024
    private let directory: URL

    init(directory: URL = SoloistRuntimeInstaller.runtimeDirectory) { self.directory = directory }

    func install(_ source: URL) throws {
        let scoped = source.startAccessingSecurityScopedResource()
        defer { if scoped { source.stopAccessingSecurityScopedResource() } }
        let (input, size) = try Self.openExecutable(source)
        defer { try? input.close() }
        try Task.checkCancellation()

        let manager = FileManager.default
        try manager.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        var info = stat()
        guard lstat(directory.path, &info) == 0, info.st_mode & S_IFMT == S_IFDIR else {
            throw SoloistFailure(message: "Auvra’s playback folder is unavailable.")
        }
        let staging = directory.appendingPathComponent(".install-" + UUID().uuidString, isDirectory: true)
        try manager.createDirectory(at: staging, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        defer { try? manager.removeItem(at: staging) }
        let candidate = staging.appendingPathComponent("soloist", isDirectory: false)
        let descriptor = open(candidate.path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0o700)
        guard descriptor >= 0 else { throw CocoaError(.fileWriteUnknown) }
        let output = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? output.close() }
        var remaining = size
        while remaining > 0 {
            try Task.checkCancellation()
            guard let chunk = try input.read(upToCount: min(remaining, 1024 * 1024)), !chunk.isEmpty else {
                throw CocoaError(.fileReadCorruptFile)
            }
            try output.write(contentsOf: chunk)
            remaining -= chunk.count
        }
        guard try input.read(upToCount: 1)?.isEmpty != false else { throw CocoaError(.fileReadCorruptFile) }
        // Keep the download's extended attributes, including quarantine.
        guard fcopyfile(input.fileDescriptor, descriptor, nil, copyfile_flags_t(COPYFILE_XATTR)) == 0 else {
            throw CocoaError(.fileWriteUnknown)
        }
        try output.synchronize()
        try Self.validate(candidate)
        try Task.checkCancellation()
        let destination = directory.appendingPathComponent("soloist", isDirectory: false)
        guard rename(candidate.path, destination.path) == 0 else { throw CocoaError(.fileWriteUnknown) }
    }

    private static func openExecutable(_ url: URL) throws -> (FileHandle, Int) {
        let issue = SoloistFailure(message: "Choose the extracted Apple Silicon soloist executable from Spotify’s official download.")
        guard url.isFileURL else { throw issue }
        let descriptor = open(url.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
        guard descriptor >= 0 else { throw issue }
        var info = stat()
        guard fstat(descriptor, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
              (4...Int64(maximumBytes)).contains(info.st_size) else {
            close(descriptor)
            throw issue
        }
        return (FileHandle(fileDescriptor: descriptor, closeOnDealloc: true), Int(info.st_size))
    }

    nonisolated static func validate(_ executable: URL) throws {
        let (file, _) = try openExecutable(executable)
        defer { try? file.close() }
        let magic = try file.read(upToCount: 4)
        let formats = [Data([0xcf, 0xfa, 0xed, 0xfe]), Data([0xca, 0xfe, 0xba, 0xbe]), Data([0xca, 0xfe, 0xba, 0xbf])]
        var code: SecStaticCode?
        var requirement: SecRequirement?
        let architecture = [kSecCodeAttributeArchitecture as String: "arm64"] as CFDictionary
        guard let magic, formats.contains(magic),
              SecStaticCodeCreateWithPathAndAttributes(executable as CFURL, [], architecture, &code) == errSecSuccess,
              SecRequirementCreateWithString("anchor apple generic and identifier \"com.spotify.soloist\" and certificate leaf[subject.OU] = \"2FNC3A47ZF\"" as CFString, [], &requirement) == errSecSuccess,
              let code, let requirement,
              SecStaticCodeCheckValidity(code, SecCSFlags(rawValue: UInt32(kSecCSCheckAllArchitectures)), requirement) == errSecSuccess else {
            throw SoloistFailure(message: "The file is not a signed Spotify Soloist executable for Apple Silicon.")
        }
    }
}

struct SoloistClock: Decodable, Sendable {
    var volume: Float? = nil
    var options: RemoteOutputOptions? = nil
    var actions: RemoteOutputActions? = nil
    var metadata: RemoteOutputMetadata? = nil
    var queueEpoch: UInt64? = nil
    var sourcePositionTimestampMs: Double? = nil
    fileprivate var confirmedSameMediaNext: UInt64? = nil
    let sequence: UInt64
    let mediaId: String?
    let state: PlaybackState
    let positionMs: UInt64
    let rate: Double
    let active: Bool
    let uptime: Double
    func isValid(at now: Double) -> Bool {
        guard sequence > 0 && positionMs <= 86_400_000 && rate.isFinite && (0...4).contains(rate)
            && uptime.isFinite && (-0.1...6).contains(now - uptime)
            && [.idle,.loading,.playing,.paused].contains(state)
            && (volume == nil || (volume!.isFinite && (0...1).contains(volume!)))
            && actions?.isValid != false
            else { return false }
        guard let mediaId else { return state == .idle }
        let parts = mediaId.split(separator: ":", omittingEmptySubsequences: false)
        return parts.count == 3 && parts[0] == "spotify" && ["track","episode","chapter"].contains(parts[1])
            && parts[2].utf8.count == 22 && parts[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) })
    }
    func observation(device: String, generation: UInt64, controlSupported: Bool = false,
                     confirmedSameMediaNext: UInt64? = nil) -> RemoteOutputClock {
        let age = ProcessInfo.processInfo.systemUptime - uptime
        let elapsed = state == .playing ? max(0, age) * rate * 1000 : 0
        let controlUptime = controlSupported && uptime >= 0 && age <= 0.9 ? UInt64(uptime * 1000) : nil
        return RemoteOutputClock(volume: volume, options: options, actions: actions, metadata: metadata, controlUptimeMs: controlUptime, queueEpoch: queueEpoch,
                                 confirmedSameMediaNext: confirmedSameMediaNext, generation: generation, sequence: sequence, deviceId: device, mediaId: mediaId, state: state,
                                 positionMs: min(86_400_000, positionMs + UInt64(elapsed)), rate: rate, active: active)
    }
}

/// UI equality deliberately excludes freshness, sequence and queue epoch. Those
/// fields remain in latestClock and are consumed on every observed callback.
private enum SoloistClockPresentation {
    static func optional<T>(_ lhs: T?, _ rhs: T?, equal: (T, T) -> Bool) -> Bool {
        switch (lhs, rhs) {
        case (.none, .none): true
        case let (.some(lhs), .some(rhs)): equal(lhs, rhs)
        default: false
        }
    }
    static func matches(_ lhs: SoloistClock?, _ rhs: SoloistClock?) -> Bool {
        optional(lhs, rhs) { a, b in
            guard a.mediaId == b.mediaId, a.state == b.state, a.rate == b.rate,
                  a.active == b.active, a.volume == b.volume else { return false }
            guard optional(a.metadata, b.metadata, equal: { a, b in
                a.title == b.title && a.artists == b.artists && a.album == b.album
                    && a.durationMs == b.durationMs && a.artwork == b.artwork && a.contextUri == b.contextUri
            }), optional(a.options, b.options, equal: { a, b in
                a.shuffle == b.shuffle && a.repeat == b.repeat && a.enhancement == b.enhancement
            }), optional(a.actions, b.actions, equal: { a, b in
                a.play == b.play && a.pause == b.pause && a.seek == b.seek && a.next == b.next
                    && a.previous == b.previous && a.shuffle == b.shuffle && a.repeat == b.repeat
                    && a.queueAdd == b.queueAdd && a.seekForwardMs == b.seekForwardMs && a.seekBackwardMs == b.seekBackwardMs
            }) else { return false }
            guard a.state == .playing else { return a.positionMs == b.positionMs }
            let duration = a.metadata?.durationMs ?? 0
            let expected = min(Double(duration > 0 ? duration : 86_400_000),
                               Double(a.positionMs) + max(0, b.uptime - a.uptime) * a.rate * 1000)
            return abs(Double(b.positionMs) - expected) <= 250
        }
    }
}
struct SoloistQueue: Decodable, Sendable {
    let epoch, sequence: UInt64
    let mediaId: String
    let uptime: Double
    let previous, upcoming: [RemoteOutputQueueEntry]
    var autoplayReady: Bool? = nil
    func matches(_ clock: SoloistClock) -> Bool {
        clock.active && clock.queueEpoch == epoch && clock.mediaId == mediaId
    }
    func isFresh(at now: Double) -> Bool {
        epoch > 0 && sequence > 0 && uptime.isFinite && (-0.1...18).contains(now - uptime)
            && previous.count <= 10 && upcoming.count <= 10
    }
}

/// Only the bundled supervisor owns the official runtime. All blocking process and
/// filesystem work runs here, away from AppKit and the player control queue.
protocol SoloistProcessManaging: Sendable {
    func launch(key: Data, name: String, initialVolume: UInt8, generation: UInt64, receive: @escaping @Sendable (Data) -> Void) async throws
    func stop(generation: UInt64) async
    func forgetSession(generation: UInt64) async throws
    func sendControl(_ command: RemoteOutputCommand, wireID: UInt64) async -> Bool
    func sendActivation(id: UInt64, expiresAtUptimeMs: UInt64, expiresAtContinuousNs: UInt64, generation: UInt64) async -> Bool
    func cancelActivation(id: UInt64, generation: UInt64) async
}
extension SoloistProcessManaging {
    func sendActivation(id: UInt64, expiresAtUptimeMs: UInt64, expiresAtContinuousNs: UInt64, generation: UInt64) async -> Bool { false }
    func cancelActivation(id: UInt64, generation: UInt64) async {}
}
/// Read one available chunk per notification. FileHandle.read(upToCount:) can
/// wait for the requested size on a live pipe, delaying tiny startup events.
/// The source owns a duplicate descriptor until its cancel handler runs, so
/// teardown cannot race a read against a closed/reused FileHandle descriptor.
final class SoloistOutputReader: @unchecked Sendable {
    private let source: any DispatchSourceRead
    private let descriptor: Int32
    private let receive: @Sendable (Data) -> Void
    private let finished: @Sendable () -> Void
    init(handle: FileHandle, receive: @escaping @Sendable (Data) -> Void, finished: @escaping @Sendable () -> Void) throws {
        let descriptor = dup(handle.fileDescriptor)
        guard descriptor >= 0 else { throw POSIXError(.EIO) }
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFD, FD_CLOEXEC) == 0,
              fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
            let code = POSIXErrorCode(rawValue: errno) ?? .EIO
            close(descriptor); throw POSIXError(code)
        }
        self.descriptor = descriptor; self.receive = receive; self.finished = finished
        source = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: DispatchQueue(label: "app.auvra.playback-output", qos: .utility))
        source.setCancelHandler { close(descriptor) }
        source.setEventHandler { [weak self] in self?.readAvailable() }
        source.activate()
    }
    private func readAvailable() {
        var buffer = [UInt8](repeating: 0, count: 4096)
        let count = Darwin.read(descriptor, &buffer, buffer.count)
        if count > 0 { receive(Data(buffer.prefix(count))) }
        else if count == 0 || (errno != EAGAIN && errno != EINTR) { source.cancel(); finished() }
    }
    func cancel() { source.cancel() }
    deinit { source.cancel() }
}
actor SoloistProcessOwner: SoloistProcessManaging {
    private struct PipeLaunch: Encodable {
        let executable, dataDirectory, cacheDirectory, deviceName, apiKey: String
        let initialVolume: UInt8
    }
    private struct PipeControl: Encodable {
        let id, sequence, expiresAtUptimeMs: UInt64
        let requiredQueueEpoch: UInt64?
        let mediaId: String
        let control: RemoteOutputCommand.Control
    }
    private struct PipeActivation: Encodable {
        let type = "bootstrap"
        let action: String
        let id, expiresAtUptimeMs: UInt64
        let expiresAtContinuousNs: UInt64
    }
    private var process: Process?
    private var input: FileHandle?
    private var output: FileHandle?
    private var outputReader: SoloistOutputReader?
    private var latestGeneration: UInt64 = 0

    func launch(key: Data, name: String, initialVolume: UInt8, generation: UInt64, receive: @escaping @Sendable (Data) -> Void) throws {
        guard generation >= latestGeneration else { throw CancellationError() }
        latestGeneration = generation; stopProcess()
        let manager = FileManager.default
        let executable = SoloistRuntimeInstaller.runtimeDirectory.appendingPathComponent("soloist")
        do { try SoloistRuntimeInstaller.validate(executable) }
        catch { throw SoloistFailure(message: "Install the official Apple Silicon Soloist executable in Settings → Account.") }
        let data = SoloistRuntimeInstaller.supportDirectory.appendingPathComponent("SpotifyPlayback", isDirectory: true)
        let cache = manager.urls(for: .cachesDirectory, in: .userDomainMask)[0].appendingPathComponent("Auvra/SpotifyPlayback", isDirectory: true)
        for directory in [data, cache] {
            try manager.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            try manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
        }
        guard let apiKey = String(data: key, encoding: .utf8), !apiKey.isEmpty, apiKey.utf8.count <= 8192, !apiKey.contains("\0") else { throw SoloistFailure(message: "Enter a valid Soloist API key.") }
        let child = Process()
        child.executableURL = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/AuvraPlaybackHost")
        let incoming = Pipe(); let outgoing = Pipe()
        child.standardInput = incoming; child.standardOutput = outgoing; child.standardError = FileHandle.nullDevice
        child.terminationHandler = { process in
            receive(Data("{\"event\":\"supervisor_exited\",\"code\":\(process.terminationStatus)}\n".utf8))
        }
        // The helper's output is a tiny, sanitized protocol, never runtime logs.
        let reader = try SoloistOutputReader(handle: outgoing.fileHandleForReading, receive: receive,
            finished: { receive(Data("{\"event\":\"output_closed\"}\n".utf8)) })
        do { try child.run() } catch {
            reader.cancel()
            throw SoloistFailure(message: "The Auvra playback helper could not start. Reinstall or rebuild Auvra.")
        }
        process = child; input = incoming.fileHandleForWriting; output = outgoing.fileHandleForReading; outputReader = reader
        let request = PipeLaunch(executable: executable.path, dataDirectory: data.path, cacheDirectory: cache.path,
                                 deviceName: name, apiKey: apiKey, initialVolume: initialVolume)
        do {
            var payload = try JSONEncoder().encode(request); payload.append(10)
            try incoming.fileHandleForWriting.write(contentsOf: payload)
            payload.resetBytes(in: payload.startIndex..<payload.endIndex)
        } catch { stopProcess(); throw SoloistFailure(message: "The playback helper could not receive its configuration.") }
    }
    func sendControl(_ command: RemoteOutputCommand, wireID: UInt64) -> Bool {
        guard command.generation == latestGeneration, let input, process?.isRunning == true else { return false }
        let request = PipeControl(id: wireID, sequence: command.sequence, expiresAtUptimeMs: command.expiresAtUptimeMs,
                                  requiredQueueEpoch: command.requiredQueueEpoch,
                                  mediaId: command.mediaId, control: command.control)
        return write(request, input: input)
    }
    func sendActivation(id: UInt64, expiresAtUptimeMs: UInt64, expiresAtContinuousNs: UInt64, generation: UInt64) -> Bool {
        let continuous = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
        guard !Task.isCancelled, generation == latestGeneration, let input, process?.isRunning == true,
              expiresAtUptimeMs >= UInt64(ProcessInfo.processInfo.systemUptime * 1000),
              expiresAtContinuousNs >= continuous, expiresAtContinuousNs - continuous <= 1_500_000_000 else { return false }
        return write(PipeActivation(action: "activate", id: id, expiresAtUptimeMs: expiresAtUptimeMs,
                                    expiresAtContinuousNs: expiresAtContinuousNs), input: input)
    }
    func cancelActivation(id: UInt64, generation: UInt64) {
        guard generation == latestGeneration, let input, process?.isRunning == true else { return }
        _ = write(PipeActivation(action: "cancel", id: id, expiresAtUptimeMs: 0, expiresAtContinuousNs: 0), input: input)
    }
    private func write<T: Encodable>(_ request: T, input: FileHandle) -> Bool {
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        guard var payload = try? encoder.encode(request), payload.count < 512 else { return false }
        payload.append(10)
        let fd = input.fileDescriptor
        let flags = fcntl(fd, F_GETFL)
        guard flags >= 0, fcntl(fd, F_SETNOSIGPIPE, 1) == 0, fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0 else { return false }
        // Keep each write within PIPE_BUF so a full pipe rejects the whole command.
        return payload.withUnsafeBytes { Darwin.write(fd, $0.baseAddress, $0.count) == $0.count }
    }
    func stop(generation: UInt64) {
        guard generation >= latestGeneration else { return }
        latestGeneration = generation; stopProcess()
    }
    private func stopProcess() {
        outputReader?.cancel(); outputReader = nil
        try? input?.close(); input = nil
        // EOF makes the supervisor terminate its actual child, including after a crash.
        process?.waitUntilExit(); process = nil
        try? output?.close(); output = nil
    }
    func forgetSession(generation: UInt64) throws {
        guard generation >= latestGeneration else { throw CancellationError() }
        stop(generation: generation)
        let directory = SoloistRuntimeInstaller.supportDirectory.appendingPathComponent("SpotifyPlayback")
        if FileManager.default.fileExists(atPath: directory.path) { try FileManager.default.removeItem(at: directory) }
    }
}
private struct SoloistFailure: LocalizedError, Sendable { let message: String; var errorDescription: String? { message } }

@MainActor @Observable
final class SoloistRuntime {
    private static let tracePlayback = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
    private static let playbackTiming = Logger(subsystem: "app.auvra.player", category: "PlaybackTiming")
    private enum ControlFinishCause: String {
        case receipt, timeout, writeFailed = "write_failed", scopeChanged = "scope_changed"
        case clockCleared = "clock_cleared", inactive
    }
    private struct PendingControl {
        let wireID, generation: UInt64
        let processID: Int32
        let kind: RemoteOutputCommand.Control.Kind
        let completion: @MainActor (Bool) -> Void
        var startWriteBegan = false
    }
    private struct PendingActivation {
        let id, generation: UInt64
        let processID: Int32
        let receiptDeadlineContinuousNs: UInt64
        let completion: CheckedContinuation<Bool, Never>
    }
    private struct SameMediaNext {
        let wireID: UInt64
        let command: RemoteOutputCommand
        let processID: Int32
        let sequence: UInt64
        let epoch: UInt64
        let dispatchedAt: Double
        var written = false
        var acknowledged = false
        var proof: SoloistClock?
        var proofDecodedAt: Double?

        var expires: Double { min(dispatchedAt + 1.5, Double(command.expiresAtUptimeMs) / 1_000) }

        func provesTransition(_ clock: SoloistClock, now: Double) -> Bool {
            clock.sequence > sequence && clock.queueEpoch == epoch + 1
                && (dispatchedAt...now).contains(clock.uptime) && now - clock.uptime <= 0.9
                && clock.state == .playing && clock.rate > 0 && clock.actions != nil
                && clock.positionMs <= 1_500
        }
    }
    struct ChildInterruption: Sendable, Equatable {
        let generation: UInt64
        let processID: Int32
    }
    private static let localControlIssue = "The local playback connection is unavailable. Try restarting the engine."
    private(set) var status = SoloistStatus()
    // Transport always reads the newest packet. Only presentation changes enter
    // Observation; a heartbeat must not re-layout the whole player window.
    @ObservationIgnored private var latestClock: SoloistClock?
    private var clockPresentationRevision: UInt64 = 0
    private(set) var clock: SoloistClock? {
        get { _ = clockPresentationRevision; return latestClock }
        set {
            let changed = !SoloistClockPresentation.matches(latestClock, newValue)
            latestClock = newValue
            if changed { clockPresentationRevision &+= 1 }
        }
    }
    private(set) var queue: SoloistQueue?
    var clockGeneration: UInt64 { generation }
    private(set) var isForgetting = false
    private(set) var isInstalling = false
    private(set) var installIssue: String?
    private(set) var installNotice: String?
    let deviceName: String
    @ObservationIgnored private let credentials: any CredentialStore
    @ObservationIgnored private let owner: any SoloistProcessManaging
    @ObservationIgnored private let installer: any SoloistInstalling
    @ObservationIgnored private var installTask: Task<Void, Never>?
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var outputTask: Task<Void, Never>?
    @ObservationIgnored private var generation: UInt64 = 0
    @ObservationIgnored private var buffer = Data()
    @ObservationIgnored private var retries = 0
    @ObservationIgnored private var ended = false
    @ObservationIgnored private var streamClosed = false
    // Pipe EOF can clear the visible PID before the supervisor confirms that
    // its actual child exited. Keep the launch identity until generation reset.
    @ObservationIgnored private var launchedProcessID: Int32?
    @ObservationIgnored private var reportedChildExit = false
    @ObservationIgnored private var lastClockSequence: UInt64 = 0
    @ObservationIgnored private var scopedPlayPausedSince: UInt64?
    @ObservationIgnored private var lastQueueSequence: UInt64 = 0
    @ObservationIgnored private var controlSerial: UInt64 = 0
    @ObservationIgnored private var pendingControl: PendingControl?
    @ObservationIgnored private var controlTimeout: Task<Void, Never>?
    @ObservationIgnored private var pendingActivation: PendingActivation?
    @ObservationIgnored private var activationWriter: Task<Void, Never>?
    @ObservationIgnored private var activationTimeout: Task<Void, Never>?
    @ObservationIgnored private var sameMediaNext: SameMediaNext?
    @ObservationIgnored private var sameMediaNextTimeout: Task<Void, Never>?
    @ObservationIgnored private var projectedSameMediaNext: (id: UInt64, device: String)?
    @ObservationIgnored private var clockExpiry: Task<Void, Never>?
    @ObservationIgnored var changed: (@MainActor () -> Void)?
    @ObservationIgnored var observed: (@MainActor () -> Void)?
    @ObservationIgnored var interrupted: (@MainActor (ChildInterruption) -> Void)?
    @ObservationIgnored var resetting: (@MainActor () -> Void)?
    @ObservationIgnored private let volumeDefaults: UserDefaults
    @ObservationIgnored private let activationContinuousTime: @Sendable () -> UInt64
    private static let volumeKey = "AuvraSoloistVolumePercent"
    init(credentials: any CredentialStore = KeychainCredentialStore(), owner: any SoloistProcessManaging = SoloistProcessOwner(),
         deviceName: String? = nil, installer: any SoloistInstalling = SoloistRuntimeInstaller(), volumeDefaults: UserDefaults = .standard,
         activationContinuousTime: @escaping @Sendable () -> UInt64 = { clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW) }) {
        self.credentials = credentials; self.owner = owner; self.installer = installer
        self.volumeDefaults = volumeDefaults
        self.activationContinuousTime = activationContinuousTime
        if let deviceName { self.deviceName = deviceName; return }
        let suffix = UserDefaults.standard.string(forKey: "AuvraPlaybackInstallation") ?? String(UUID().uuidString.prefix(8))
        UserDefaults.standard.set(suffix, forKey: "AuvraPlaybackInstallation")
        self.deviceName = "Auvra · \(suffix)"
    }
    func rememberVolume(_ value: Double) {
        guard value.isFinite, (0...1).contains(value) else { return }
        volumeDefaults.set(Int((value * 100).rounded()), forKey: Self.volumeKey)
    }
    private var initialVolume: UInt8 {
        let stored = volumeDefaults.object(forKey: Self.volumeKey) as? Int ?? 40
        return (0...100).contains(stored) ? UInt8(stored) : 40
    }
    func restore() { guard !isForgetting, !isInstalling, task == nil, status.processID == nil, status.phase != .starting else { return }; start() }
    func installRuntime(from source: URL) {
        guard !isForgetting, !isInstalling else { return }
        isInstalling = true; installIssue = nil; installNotice = nil
        installTask = Task { [weak self, installer] in
            let issue: String?
            do {
                try await installer.install(source)
                issue = nil
            } catch {
                issue = (error as? SoloistFailure)?.message ?? "Auvra could not install the playback engine. Check the file and available storage, then try again."
            }
            guard let self, !Task.isCancelled else { return }
            installTask = nil; isInstalling = false
            if let issue { installIssue = issue; return }
            if status.processID != nil || status.phase == .starting {
                installNotice = "Playback engine installed. Choose Reconnect Playback to use it."
            } else {
                installNotice = "Playback engine installed."
                retries = 0; start()
            }
        }
    }
    func configure(key: String) {
        guard !isForgetting, !isInstalling else { return }
        let key = key.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty, key.utf8.count <= 8192, !key.contains("\0") else { set(.failed, "Enter a valid Soloist API key."); return }
        resetting?(); finishActivation(accepted: false, cancelWire: true)
        generation &+= 1; let token = generation; task?.cancel(); outputTask?.cancel()
        launchedProcessID = nil; reportedChildExit = false
        clearClock(); status.active = nil
        status.clockSupported = false; status.controlSupported = false
        status.processID = nil; status.authenticated = nil; set(.starting)
        task = Task { [weak self] in
            guard let self else { return }
            await owner.stop(generation: token)
            do { try await credentials.write(Data(key.utf8), account: "spotify-soloist") }
            catch { guard generation == token else { return }; task = nil; set(.failed, "Keychain could not save the playback key."); return }
            guard generation == token, !Task.isCancelled else { return }
            task = nil; retries = 0; start()
        }
    }
    func retry() { guard !isForgetting, !isInstalling else { return }; resetting?(); installNotice = nil; retries = 0; start() }
    private func start() {
        finishActivation(accepted: false, cancelWire: true)
        generation &+= 1; let token = generation; task?.cancel(); outputTask?.cancel()
        buffer.removeAll(keepingCapacity: true); ended = false; streamClosed = false
        launchedProcessID = nil; reportedChildExit = false
        clearClock(); lastClockSequence = 0; lastQueueSequence = 0; status.active = nil
        status.clockSupported = false; status.controlSupported = false
        status.processID = nil; status.authenticated = nil; set(.starting)
        task = Task { [weak self] in
            guard let self else { return }
            do {
                await owner.stop(generation: token)
                guard generation == token, !Task.isCancelled else { return }
                let savedKey = try await credentials.read(account: "spotify-soloist")
                guard generation == token, !Task.isCancelled else { return }
                guard let key = savedKey else { task = nil; set(.notConfigured); return }
                let tracePlayback = Self.tracePlayback
                let output = AsyncStream<(data: Data, receivedAt: Double?)>.makeStream(bufferingPolicy: .bufferingOldest(32))
                outputTask = Task { [weak self] in
                    for await chunk in output.stream {
                        self?.receive(chunk.data, generation: token, receivedAt: chunk.receivedAt)
                    }
                    guard !Task.isCancelled, let self, generation == token, !ended, !streamClosed else { return }
                    finishActivation(accepted: false, cancelWire: true)
                    streamClosed = true; status.processID = nil; status.authenticated = nil; clearClock()
                    set(.failed, "The playback helper sent too much data. Try restarting the engine.")
                    await owner.stop(generation: token)
                }
                try await owner.launch(key: key, name: deviceName, initialVolume: initialVolume, generation: token) { data in
                    let receivedAt = tracePlayback ? ProcessInfo.processInfo.systemUptime : nil
                    if case .dropped = output.continuation.yield((data, receivedAt)) { output.continuation.finish() }
                }
                guard generation == token else { return }; if !ended { task = nil }
            } catch {
                guard generation == token else { return }; task = nil
                outputTask?.cancel()
                set(.failed, (error as? SoloistFailure)?.message ?? "The playback engine could not start. Check Keychain access and try again.")
            }
        }
    }
    private func receive(_ data: Data, generation token: UInt64, receivedAt: Double?) {
        guard generation == token else { return }
        let actorWaitMs = receivedAt.map { (ProcessInfo.processInfo.systemUptime - $0) * 1000 }
        guard !data.isEmpty, data.count <= 4096 else {
            finishActivation(accepted: false, cancelWire: true)
            clearClock(); set(.failed, "The playback helper returned an invalid response."); return
        }
        buffer.append(data)
        while let newline = buffer.firstIndex(of: 10) {
            let line = buffer[..<newline]; buffer.removeSubrange(...newline)
            guard line.count <= 48 * 1024 else {
                finishActivation(accepted: false, cancelWire: true)
                clearClock(); buffer.removeAll(); set(.failed, "The playback helper returned an invalid response."); return
            }
            guard let message = try? JSONSerialization.jsonObject(with: line) as? [String: Any], let event = message["event"] as? String else { continue }
            switch event {
            case "started":
                guard !ended, !streamClosed, let pid = message["pid"] as? Int32, pid > 0 else { continue }
                guard launchedProcessID == nil || launchedProcessID == pid else { continue }
                launchedProcessID = pid
                if let previous = status.processID, previous != pid { clearClock() }
                status.clockSupported = message["clock_protocol"] as? Int == 1
                status.controlSupported = message["control_protocol"] as? Int == 1
                status.processID = pid; set(.pairing)
            case "control_result":
                guard let id = message["id"] as? UInt64, let accepted = message["accepted"] as? Bool else { continue }
                finishControl(id, accepted: accepted)
            case "activation_result":
                guard let id = message["id"] as? UInt64, let accepted = message["accepted"] as? Bool,
                      let pending = pendingActivation, pending.id == id,
                      pending.generation == token, pending.processID == launchedProcessID,
                      pending.processID == status.processID, status.authenticated == true,
                      !ended, !streamClosed else { continue }
                guard activationContinuousTime() <= pending.receiptDeadlineContinuousNs else {
                    finishActivation(accepted: false, cancelWire: true); continue
                }
                finishActivation(accepted: accepted)
            case "auth":
                guard !ended, !streamClosed, status.processID != nil, let authenticated = message["logged_in"] as? Bool else { continue }
                if !authenticated { finishActivation(accepted: false, cancelWire: true) }
                if !authenticated || status.clockSupported { status.active = false; clearClock() }
                status.authenticated = authenticated; set(authenticated ? .ready : .pairing)
            case "clock":
                guard !ended, !streamClosed, status.processID != nil, status.authenticated == true else { continue }
                guard let value = message["clock"] as? [String: Any] else {
                    // A URI start may briefly report an unidentified item. Drop
                    // ownership immediately, but keep its separately scoped wire
                    // receipt until its ACK, write failure or original deadline.
                    clearClock(preservingStartReceiptForClockGap: message["clock"] is NSNull)
                    continue
                }
                let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
                guard let bytes = try? JSONSerialization.data(withJSONObject: value), let next = try? decoder.decode(SoloistClock.self, from: bytes),
                      next.isValid(at: ProcessInfo.processInfo.systemUptime), next.sequence > lastClockSequence else { continue }
                lastClockSequence = next.sequence
                acceptClock(next, receivedAt: receivedAt, actorWaitMs: actorWaitMs)
            case "queue":
                guard !ended, !streamClosed, status.authenticated == true, let clock,
                      let value = message["queue"] as? [String: Any] else { continue }
                let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
                guard let bytes = try? JSONSerialization.data(withJSONObject: value),
                      let next = try? decoder.decode(SoloistQueue.self, from: bytes),
                      next.isFresh(at: ProcessInfo.processInfo.systemUptime), next.sequence > lastQueueSequence,
                      next.matches(clock) else { continue }
                lastQueueSequence = next.sequence; queue = next
                observed?()
            case "local_control_unavailable": if !ended && !streamClosed {
                finishActivation(accepted: false, cancelWire: true)
                clearClock(); set(status.phase, Self.localControlIssue)
            }
            case "output_closed":
                guard !ended else { continue }; streamClosed = true
                finishActivation(accepted: false, cancelWire: true)
                status.active = nil; status.processID = nil; status.authenticated = nil; clearClock()
                set(.failed, "The playback helper connection closed. Try restarting the engine.")
            case "exited", "supervisor_exited":
                finishActivation(accepted: false, cancelWire: true)
                // Only the child termination event proves the sound source
                // stopped. EOF and supervisor termination are not that proof.
                if event == "exited", !reportedChildExit, let processID = launchedProcessID {
                    reportedChildExit = true
                    interrupted?(ChildInterruption(generation: token, processID: processID))
                }
                guard !ended else { continue }; ended = true
                clearClock(); status.active = nil
                status.processID = nil; status.authenticated = nil
                if message["code"] as? Int == 10 { set(.expired, "This Soloist build has expired. Install the current official runtime and try again.") }
                else if retries < 3 {
                    retries += 1; set(.starting, "Reconnecting the playback engine…")
                    task = Task { [weak self] in
                        try? await Task.sleep(for: .seconds(2))
                        guard let self, generation == token, !Task.isCancelled else { return }; task = nil; start()
                    }
                } else { set(.failed, "The playback engine stopped. Check your Soloist key and try again.") }
            case "launch_failed":
                finishActivation(accepted: false, cancelWire: true)
                ended = true; clearClock(); status.active = nil; status.processID = nil; set(.failed, "The official playback runtime could not launch.")
            default: break
            }
        }
        if buffer.count > 48 * 1024 {
            finishActivation(accepted: false, cancelWire: true)
            clearClock(); buffer.removeAll(); set(.failed, "The playback helper returned an invalid response.")
        }
    }
    private func acceptClock(_ packet: SoloistClock, receivedAt: Double?, actorWaitMs: Double?) {
        var next = packet
        let wireID = next.confirmedSameMediaNext
        next.confirmedSameMediaNext = nil
        let now = ProcessInfo.processInfo.systemUptime
        if let wireID, let receivedAt, let actorWaitMs {
            let written = sameMediaNext?.wireID == wireID && sameMediaNext?.written == true
            let acknowledged = sameMediaNext?.wireID == wireID && sameMediaNext?.acknowledged == true
            Self.playbackTiming.info("owned_next_proof_received wire=\(wireID, privacy: .public) sequence=\(next.sequence, privacy: .public) pipe_ms=\((receivedAt - next.uptime) * 1000, privacy: .public) host_decode_ms=\((now - receivedAt) * 1000, privacy: .public) actor_wait_ms=\(actorWaitMs, privacy: .public) written=\(written, privacy: .public) ack=\(acknowledged, privacy: .public)")
        }
        guard var candidate = sameMediaNext, now <= candidate.expires,
              next.active, next.mediaId == candidate.command.mediaId,
              next.queueEpoch == candidate.epoch || next.queueEpoch == candidate.epoch + 1 else {
            clearSameMediaNext()
            publishClock(next)
            return
        }
        let hasMatchingWitness = wireID == candidate.wireID || (wireID == nil && candidate.proof != nil)
        if hasMatchingWitness && candidate.provesTransition(next, now: now) {
            candidate.proof = next
            candidate.proofDecodedAt = receivedAt.map { _ in now }
            sameMediaNext = candidate
            // Withhold this sequence until the write and matching receipt are
            // known. A later observation at the same sequence would be stale.
            if sameMediaNextTimeout == nil {
                sameMediaNextTimeout = Task { [weak self] in
                    try? await Task.sleep(for: .seconds(max(0, candidate.expires - now)))
                    guard !Task.isCancelled else { return }
                    self?.completeSameMediaNext(candidate.wireID, allowMark: false)
                }
            }
            completeSameMediaNext(candidate.wireID)
            return
        }
        if candidate.proof != nil { clearSameMediaNext() }
        publishClock(next)
    }

    private func publishClock(_ next: SoloistClock, marker: (id: UInt64, device: String)? = nil) {
        let token = generation
        if !isPausedForScopedPlay(next) {
            scopedPlayPausedSince = nil
        } else if scopedPlayPausedSince == nil || next.mediaId != clock?.mediaId || next.queueEpoch != clock?.queueEpoch {
            scopedPlayPausedSince = next.sequence
        }
        projectedSameMediaNext = marker
        clock = next
        // A nested write invokes Observation's _modify even when Bool is equal.
        if status.active != next.active { status.active = next.active }
        if !next.active, let pending = pendingControl, pending.kind == .startUri {
            finishControl(pending.wireID, accepted: false, cause: .inactive)
        }
        if queue?.matches(next) == false { queue = nil }
        if status.issue == Self.localControlIssue { set(status.phase) }
        clockExpiry?.cancel()
        let remaining = max(0, 6 - (ProcessInfo.processInfo.systemUptime - next.uptime))
        clockExpiry = Task { [weak self] in
            try? await Task.sleep(for: .seconds(remaining))
            guard !Task.isCancelled, let self, generation == token, clock?.sequence == next.sequence else { return }
            clearClock()
        }
        observed?()
    }

    func observation(device: String) -> RemoteOutputClock? {
        let now = ProcessInfo.processInfo.systemUptime
        guard let clock, clock.isValid(at: now) else { return nil }
        let marker = projectedSameMediaNext.flatMap { next in
            status.authenticated == true && status.controlSupported && next.device == device
                && now - clock.uptime <= 0.9 ? next.id : nil
        }
        return clock.observation(device: device, generation: generation, controlSupported: status.controlSupported,
                                 confirmedSameMediaNext: marker)
    }

    private func completeSameMediaNext(_ wireID: UInt64, allowMark: Bool = true) {
        guard let candidate = sameMediaNext, candidate.wireID == wireID,
              !allowMark || (candidate.written && candidate.acknowledged && candidate.proof != nil) else { return }
        clearSameMediaNext()
        let now = ProcessInfo.processInfo.systemUptime
        guard status.authenticated == true, status.processID == candidate.processID,
              generation == candidate.command.generation, let proof = candidate.proof,
              proof.sequence == lastClockSequence, proof.isValid(at: now) else { return }
        let confirmed = allowMark && now <= candidate.expires && candidate.provesTransition(proof, now: now)
        if let decodedAt = candidate.proofDecodedAt {
            Self.playbackTiming.info("owned_next_proof_published wire=\(wireID, privacy: .public) sequence=\(proof.sequence, privacy: .public) hold_ms=\((now - decodedAt) * 1000, privacy: .public) confirmed=\(confirmed, privacy: .public)")
        }
        publishClock(proof, marker: confirmed ? (candidate.command.id, candidate.command.deviceId) : nil)
    }

    private func clearSameMediaNext() {
        sameMediaNextTimeout?.cancel(); sameMediaNextTimeout = nil; sameMediaNext = nil
    }

    private func isPausedForScopedPlay(_ clock: SoloistClock) -> Bool {
        clock.active && clock.state == .paused && clock.rate == 0 && clock.positionMs <= 120
    }
    private func matchesRequiredOccurrence(_ command: RemoteOutputCommand, ownedDeviceID: String?, at now: Double) -> Bool {
        guard let requiredEpoch = command.requiredQueueEpoch else { return command.control.kind != .previousFromStart }
        guard command.control.kind == .pause || command.control.kind == .previousFromStart || command.control.kind == .next || command.control.kind == .play,
              requiredEpoch > 0, command.deviceId == ownedDeviceID,
              command.generation == generation, status.authenticated == true, status.processID != nil, status.controlSupported,
              let clock, clock.active, clock.mediaId == command.mediaId, clock.queueEpoch == requiredEpoch,
              clock.sequence >= command.sequence, clock.isValid(at: now), now - clock.uptime <= 0.9
        else { return false }
        if command.control.kind == .play {
            // A Playing → Paused change can retain the same queue occurrence.
            // A recovery from its earlier pause cannot reopen this new pause.
            guard isPausedForScopedPlay(clock), clock.actions?.play == true,
                  let scopedPlayPausedSince, command.sequence >= scopedPlayPausedSince else { return false }
        }
        return command.expiresAtUptimeMs >= UInt64(now * 1000)
    }
    /// A launch-scoped wake-up for an explicit local selection, not a transport
    /// capability. Its receipt proves dispatch only; the caller must still find
    /// the real Connect device and confirm its submitted playback target.
    var canActivateForSelection: Bool {
        !ended && !streamClosed && pendingControl == nil && pendingActivation == nil
            && status.phase == .ready
            && status.controlSupported && status.authenticated == true && status.active != true
            && status.processID != nil && status.processID == launchedProcessID
    }
    func activateForSelection() async -> Bool {
        guard !Task.isCancelled, canActivateForSelection, let processID = launchedProcessID else { return false }
        controlSerial &+= 1
        let id = controlSerial, token = generation
        let deadline = UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 1_500
        let continuous = activationContinuousTime()
        let (continuousDeadline, overflow) = continuous.addingReportingOverflow(1_500_000_000)
        let (receiptDeadline, receiptOverflow) = continuous.addingReportingOverflow(2_500_000_000)
        guard !overflow, !receiptOverflow else { return false }
        return await withTaskCancellationHandler {
            await withCheckedContinuation { completion in
                guard !Task.isCancelled, canActivateForSelection, generation == token,
                      launchedProcessID == processID else { completion.resume(returning: false); return }
                pendingActivation = PendingActivation(id: id, generation: token, processID: processID,
                                                      receiptDeadlineContinuousNs: receiptDeadline, completion: completion)
                activationTimeout = Task { [weak self] in
                    do { try await Task.sleep(for: .milliseconds(2500)) } catch { return }
                    guard let self, pendingActivation?.id == id else { return }
                    finishActivation(accepted: false, cancelWire: true)
                }
                activationWriter = Task { [weak self, owner] in
                    guard !Task.isCancelled, let self, pendingActivation?.id == id,
                          generation == token, launchedProcessID == processID,
                          status.processID == processID, status.authenticated == true,
                          !ended, !streamClosed else { return }
                    let written = await owner.sendActivation(id: id, expiresAtUptimeMs: deadline,
                                                             expiresAtContinuousNs: continuousDeadline, generation: token)
                    guard pendingActivation?.id == id else { return }
                    if !written { finishActivation(accepted: false, cancelWire: true) }
                }
            }
        } onCancel: { [weak self] in
            Task { @MainActor in
                guard let self, self.pendingActivation?.id == id else { return }
                self.finishActivation(accepted: false, cancelWire: true)
            }
        }
    }
    private func finishActivation(accepted: Bool, cancelWire: Bool = false) {
        guard let pending = pendingActivation else { return }
        pendingActivation = nil
        activationTimeout?.cancel(); activationTimeout = nil
        activationWriter?.cancel(); activationWriter = nil
        if cancelWire {
            Task { [owner] in await owner.cancelActivation(id: pending.id, generation: pending.generation) }
        }
        if Self.tracePlayback {
            Self.playbackTiming.info("owned_activation_finish wire=\(pending.id, privacy: .public) accepted=\(accepted, privacy: .public) cancelled=\(cancelWire, privacy: .public)")
        }
        pending.completion.resume(returning: accepted)
    }
    func send(_ command: RemoteOutputCommand, ownedDeviceID: String? = nil, admission: (@MainActor () -> Bool)? = nil,
              completion: @escaping @MainActor (Bool) -> Void) {
        let now = ProcessInfo.processInfo.systemUptime
        guard pendingControl == nil, pendingActivation == nil, command.id > 0, command.generation == generation,
              status.controlSupported, status.authenticated == true, let processID = status.processID,
              admission?() != false,
              matchesRequiredOccurrence(command, ownedDeviceID: ownedDeviceID, at: now) else { completion(false); return }
        if let pending = sameMediaNext { completeSameMediaNext(pending.wireID, allowMark: false) }
        guard let clock, clock.active, clock.mediaId != nil, clock.sequence >= command.sequence,
              command.control.kind.isOutputTransport || clock.mediaId == command.mediaId,
              clock.isValid(at: now), now - clock.uptime <= 0.9,
              command.expiresAtUptimeMs >= UInt64(now * 1000) else { completion(false); return }
        controlSerial &+= 1
        let wireID = controlSerial
        projectedSameMediaNext = nil
        if command.control.kind == .next, clock.mediaId == command.mediaId,
           let epoch = clock.queueEpoch, epoch > 0, epoch < UInt64.max {
            sameMediaNext = SameMediaNext(wireID: wireID, command: command, processID: processID,
                                         sequence: clock.sequence, epoch: epoch, dispatchedAt: now)
        }
        pendingControl = PendingControl(wireID: wireID, generation: generation, processID: processID,
                                        kind: command.control.kind, completion: completion)
        controlTimeout = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(2500))
            guard !Task.isCancelled else { return }
            self?.finishControl(wireID, accepted: false, cause: .timeout)
        }
        Task { [weak self, owner] in
            if command.requiredQueueEpoch != nil || admission != nil {
                guard let self, pendingControl?.wireID == wireID else { return }
                guard admission?() != false,
                      matchesRequiredOccurrence(command, ownedDeviceID: ownedDeviceID, at: ProcessInfo.processInfo.systemUptime) else {
                    finishControl(wireID, accepted: false, cause: .scopeChanged)
                    return
                }
            }
            if command.control.kind == .startUri {
                guard let self, let pending = pendingControl, pending.wireID == wireID else { return }
                guard pending.generation == generation, pending.processID == status.processID,
                      status.authenticated == true, !ended, !streamClosed else {
                    finishControl(wireID, accepted: false, cause: .scopeChanged)
                    return
                }
                // Mark the write invocation before suspension: its clock packet
                // can arrive before the owner returns the write outcome. Failure
                // still resolves false; a successful write alone never accepts.
                pendingControl?.startWriteBegan = true
            }
            let written = await owner.sendControl(command, wireID: wireID)
            guard let self else { return }
            if written {
                if sameMediaNext?.wireID == wireID { sameMediaNext?.written = true; completeSameMediaNext(wireID) }
            } else {
                completeSameMediaNext(wireID, allowMark: false)
                finishControl(wireID, accepted: false, cause: .writeFailed)
            }
        }
    }
    private func finishControl(_ wireID: UInt64, accepted: Bool, cause: ControlFinishCause = .receipt) {
        guard let pending = pendingControl, pending.wireID == wireID else { return }
        pendingControl = nil; controlTimeout?.cancel(); controlTimeout = nil
        if Self.tracePlayback, pending.kind.isOutputTransport || pending.kind == .startUri || pending.kind == .seek {
            Self.playbackTiming.info("owned_control_finish wire=\(wireID, privacy: .public) kind=\(pending.kind.rawValue, privacy: .public) accepted=\(accepted, privacy: .public) cause=\(cause.rawValue, privacy: .public)")
        }
        pending.completion(accepted)
        if sameMediaNext?.wireID == wireID {
            sameMediaNext?.acknowledged = accepted
            completeSameMediaNext(wireID, allowMark: accepted)
        }
    }
    private func clearClock(preservingStartReceiptForClockGap: Bool = false) {
        let preserveReceipt = preservingStartReceiptForClockGap && pendingControl.map { pending in
            pending.kind == .startUri && pending.startWriteBegan
                && pending.generation == generation && pending.processID == status.processID
                && status.authenticated == true && status.controlSupported && status.active == true
                && !ended && !streamClosed
        } == true
        clearSameMediaNext(); projectedSameMediaNext = nil
        scopedPlayPausedSince = nil
        clockExpiry?.cancel(); clockExpiry = nil; clock = nil; queue = nil; observed?()
        if let pending = pendingControl {
            if preserveReceipt {
                if Self.tracePlayback {
                    Self.playbackTiming.info("owned_start_clock_gap wire=\(pending.wireID, privacy: .public) pending=true")
                }
            } else {
                finishControl(pending.wireID, accepted: false, cause: .clockCleared)
            }
        }
    }
    func noteDeviceAvailable(_ available: Bool) {
        guard status.processID != nil, status.phase == .pairing || status.phase == .ready else { return }
        if status.clockSupported && clock == nil && status.issue == Self.localControlIssue { return }
        if available && status.authenticated != false { set(.ready) }
    }
    private func set(_ phase: SoloistPhase, _ issue: String? = nil) {
        guard status.phase != phase || status.issue != issue else { return }
        status.phase = phase; status.issue = issue; changed?()
    }
    func shutdown() async {
        resetting?(); finishActivation(accepted: false, cancelWire: true)
        installTask?.cancel(); installTask = nil; isInstalling = false; installIssue = nil; installNotice = nil
        generation &+= 1; task?.cancel(); task = nil; outputTask?.cancel(); outputTask = nil
        launchedProcessID = nil; reportedChildExit = false
        clearClock(); status.active = nil
        status.clockSupported = false; status.controlSupported = false
        // Keychain can be waiting on a system dialog. Invalidation prevents its
        // late result from launching a child without making Quit wait on the dialog.
        await owner.stop(generation: generation); status.processID = nil; status.authenticated = nil; set(.notConfigured)
    }
    func forgetKey() async {
        guard !isForgetting else { return }; isForgetting = true; defer { isForgetting = false }
        await shutdown()
        do { try await credentials.remove(account: "spotify-soloist"); try await owner.forgetSession(generation: generation) }
        catch { set(.failed, "The saved playback login could not be fully removed. Try disconnecting again.") }
    }
}
