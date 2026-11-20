import AppKit
import CryptoKit
import Darwin
import Foundation
import Observation
import Security

private func check(_ condition: @autoclosure () throws -> Bool, _ message: String) throws {
    if try !condition() { throw NSError(domain: "SpotifySmoke", code: 1, userInfo: [NSLocalizedDescriptionKey: message]) }
}
private final class ObservationCount: @unchecked Sendable {
    private let lock = NSLock()
    private var value = 0
    func record() { lock.lock(); defer { lock.unlock() }; value += 1 }
    var count: Int { lock.lock(); defer { lock.unlock() }; return value }
}
private final class BootstrapFixtureClock: @unchecked Sendable {
    private let lock = NSLock()
    private var offset: UInt64 = 0
    func read() -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        return clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW) + offset
    }
    func advance(_ value: UInt64) { lock.lock(); defer { lock.unlock() }; offset += value }
}
private actor MemoryCredentials: CredentialStore {
    var saved: Data?
    func read(account: String) -> Data? { saved }
    func write(_ value: Data, account: String) { saved = value }
    func remove(account: String) { saved = nil }
}
private actor LockedLoginCredential: CredentialStore {
    private var saved: Data?
    private var locked = true
    init(_ saved: Data) { self.saved = saved }
    func read(account: String) throws -> Data? {
        guard !locked else { throw CredentialFailure(status: errSecInteractionNotAllowed) }
        return saved
    }
    func write(_ value: Data, account: String) { saved = value }
    func remove(account: String) { saved = nil }
    func unlock() { locked = false }
}
private final class TokenFixtures: @unchecked Sendable {
    enum Reply {
        case http(Int, String, [String: String] = [:])
        case failure(URLError.Code)
    }
    static let shared = TokenFixtures()
    private let lock = NSLock()
    private var replies: [Reply] = []
    private var requests: [URLRequest] = []
    func reset(_ values: [(Int, String)]) { resetReplies(values.map { .http($0.0, $0.1) }) }
    func resetReplies(_ values: [Reply]) { lock.lock(); defer { lock.unlock() }; replies = values; requests = [] }
    func take(_ request: URLRequest) -> Reply { lock.lock(); defer { lock.unlock() }; requests.append(request); return replies.isEmpty ? .http(500, "{}") : replies.removeFirst() }
    func recorded() -> [URLRequest] { lock.lock(); defer { lock.unlock() }; return requests }
}
private final class TokenProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        var captured = request
        if captured.httpBody == nil, let stream = captured.httpBodyStream {
            stream.open(); defer { stream.close() }
            var bytes = Data(), buffer = [UInt8](repeating: 0, count: 1024)
            while bytes.count < 16384 {
                let count = stream.read(&buffer, maxLength: buffer.count)
                if count <= 0 { break }; bytes.append(contentsOf: buffer.prefix(count))
            }
            captured.httpBody = bytes
        }
        switch TokenFixtures.shared.take(captured) {
        case .http(let status, let text, let headers):
            let headers = headers.merging(["Content-Type":"application/json"], uniquingKeysWith: { a,_ in a })
            client?.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1", headerFields: headers)!, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: Data(text.utf8)); client?.urlProtocolDidFinishLoading(self)
        case .failure(let code): client?.urlProtocol(self,didFailWithError: URLError(code))
        }
    }
    override func stopLoading() {}
}
private let legacyPlaybackScopes = [
    "user-read-playback-state", "user-modify-playback-state", "user-read-currently-playing",
    "user-library-read", "playlist-read-private", "playlist-read-collaborative"
].joined(separator: " ")

private func token(_ access: String = "fixture-access", refresh: String? = "fixture-refresh", expiry: Int = 3600,
                   scope: String? = SpotifyOAuthClient.scopes.joined(separator: " ")) -> String {
    var value: [String: Any] = ["access_token": access, "token_type": "Bearer", "expires_in": expiry]
    if let scope { value["scope"] = scope }
    if let refresh { value["refresh_token"] = refresh }
    return String(data: try! JSONSerialization.data(withJSONObject: value), encoding: .utf8)!
}
private func oauthClient() -> SpotifyOAuthClient {
    let config = URLSessionConfiguration.ephemeral; config.protocolClasses = [TokenProtocol.self]
    return SpotifyOAuthClient(session: URLSession(configuration: config))
}
@MainActor private final class Observation {
    var status = SpotifyAccountStatus()
    var tokens: [String] = []
    var disconnected = 0
    var authorizationURL: URL?
    var callbackError: Error?
}
private actor PlaybackFixture: SoloistProcessManaging {
    private var receive: (@Sendable (Data) -> Void)?
    private var history: [@Sendable (Data) -> Void] = []
    private var delayWrite = false
    private var delayedWrite: CheckedContinuation<Bool, Never>?
    private(set) var starts = 0
    private(set) var commands: [UInt64] = []
    private(set) var activations: [(id: UInt64, deadline: UInt64, continuousDeadline: UInt64, generation: UInt64)] = []
    private(set) var cancelledActivations: [UInt64] = []
    private var activationWriteSucceeds = true
    func launch(key: Data, name: String, initialVolume: UInt8, generation: UInt64, receive: @escaping @Sendable (Data) -> Void) {
        starts += 1; self.receive = receive
        history.append(receive)
        receive(Data("{\"event\":\"started\",\"pid\":12345,\"clock_protocol\":1,\"control_protocol\":1}\n".utf8))
    }
    func stop(generation: UInt64) { receive = nil }
    func forgetSession(generation: UInt64) {}
    func sendControl(_ command: RemoteOutputCommand, wireID: UInt64) async -> Bool {
        commands.append(wireID)
        if !delayWrite { return true }
        delayWrite = false
        return await withCheckedContinuation { delayedWrite = $0 }
    }
    func sendActivation(id: UInt64, expiresAtUptimeMs: UInt64, expiresAtContinuousNs: UInt64, generation: UInt64) -> Bool {
        activations.append((id, expiresAtUptimeMs, expiresAtContinuousNs, generation))
        return activationWriteSucceeds
    }
    func cancelActivation(id: UInt64, generation: UInt64) { cancelledActivations.append(id) }
    func rejectActivationWrite() { activationWriteSucceeds = false }
    func delayNextWrite() { delayWrite = true }
    func finishDelayedWrite(_ written: Bool) { delayedWrite?.resume(returning: written); delayedWrite = nil }
    func event(_ message: String) { receive?(Data((message + "\n").utf8)) }
    func bytes(_ data: Data) { receive?(data) }
    func oldEvent(_ message: String) { history.first?(Data((message + "\n").utf8)) }
}
private actor InstallerFixture: SoloistInstalling {
    private(set) var attempts = 0
    func install(_ source: URL) async throws {
        attempts += 1
        try await Task.sleep(for: .milliseconds(30))
        if source.lastPathComponent == "invalid" { throw CocoaError(.fileReadCorruptFile) }
    }
}
private actor DelayedPlaybackCredentials: CredentialStore {
    var pending: CheckedContinuation<Data?, Never>?
    func read(account: String) async -> Data? { await withCheckedContinuation { pending = $0 } }
    func write(_ value: Data, account: String) {}
    func remove(account: String) {}
    func hasPendingRead() -> Bool { pending != nil }
    func finish() { pending?.resume(returning: Data("fixture-late-key".utf8)); pending = nil }
}
private final class PipeObservation: @unchecked Sendable {
    private let lock = NSLock()
    private var chunks: [Data] = []
    private var finishes = 0
    func receive(_ data: Data) { lock.lock(); defer { lock.unlock() }; chunks.append(data) }
    func finish() { lock.lock(); defer { lock.unlock() }; finishes += 1 }
    func snapshot() -> (data: Data, maximumChunk: Int, finishes: Int) {
        lock.lock(); defer { lock.unlock() }
        return (chunks.reduce(into: Data()) { $0.append($1) }, chunks.map(\.count).max() ?? 0, finishes)
    }
}
@MainActor private func eventually(_ message: String, _ predicate: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !predicate(), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
    try check(predicate(), message)
}
private func parameters(_ string: String) -> [String: String] {
    Dictionary((URLComponents(string: "https://fixture.invalid/?" + string)?.queryItems ?? []).map { ($0.name, $0.value ?? "") }, uniquingKeysWith: { a, _ in a })
}
@main struct SpotifyIntegrationSmoke {
    @MainActor static func main() async {
        do {
            if CommandLine.arguments.contains("--bootstrap-only") {
                try await testSelectionActivation()
                return
            }
            try testPKCE()
            try await testLoopback()
            try await testTokenFailures()
            try await testAuthorization()
            try await testPermissionUpdate()
            try await testRestoredLoginAndExpiry()
            try await testRefreshRecovery()
            try await testPlaybackLifecycle()
            try await testRuntimeInstaller()
            try await testInstallationLifecycle()
            try await testLivePipeDelivery()
            try await testOutputClock()
            try await testClockPresentation()
            try await testURIStartClockGap()
            try await testSameMediaNext()
            try await testScopedRecoveryPlay()
            try await testSelectionActivation()
            print("Spotify smoke passed: OAuth/Keychain fixtures, cancellation, and owned playback lifecycle/expiry. Fixtures only; live Spotify is checked separately.")
        } catch { fputs("Spotify smoke failed: \(error.localizedDescription)\n", stderr); exit(1) }
    }
    @MainActor static func testClockPresentation() async throws {
        let owner = PlaybackFixture()
        let runtime = SoloistRuntime(credentials: MemoryCredentials(), owner: owner, deviceName: "Auvra presentation fixture")
        runtime.configure(key: "fixture-presentation-key")
        try await eventually("Presentation fixture did not start") { runtime.status.processID != nil }
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
        try await eventually("Presentation fixture did not authenticate") { runtime.status.authenticated == true }
        let media = "spotify:track:" + String(repeating: "a", count: 22)
        func packet(_ sequence: UInt64, position: UInt64 = 3_500, state: String = "paused", rate: Double = 0,
                    volume: Double = 0.4, title: String = "Original title", next: Bool = true) throws -> String {
            let clock: [String: Any] = ["sequence":sequence, "state":state, "rate":rate, "active":true,
                "media_id":media, "position_ms":position, "uptime":ProcessInfo.processInfo.systemUptime,
                "queue_epoch":7, "volume":volume,
                "actions":["play":true,"pause":true,"seek":true,"next":next,"previous":true,"shuffle":true,"repeat":true,"queue_add":true],
                "metadata":["title":title,"artists":["Original artist"],"duration_ms":65_000]]
            return String(decoding: try JSONSerialization.data(withJSONObject: ["event":"clock", "clock":clock]), as: UTF8.self)
        }
        var callbacks = 0
        runtime.observed = { callbacks += 1 }
        await owner.event(try packet(1))
        try await eventually("Initial paused clock did not arrive") { runtime.clock?.sequence == 1 }
        let baseline = callbacks, originalUptime = runtime.clock!.uptime
        let paused = ObservationCount()
        withObservationTracking { _ = runtime.clock; _ = runtime.status } onChange: { paused.record() }
        for sequence in 2...51 { await owner.event(try packet(UInt64(sequence))) }
        try await eventually("Raw paused heartbeats were dropped") { runtime.clock?.sequence == 51 }
        try check(paused.count == 0, "Unchanged paused clocks or same-value status.active invalidated presentation")
        try check(callbacks - baseline == 50 && runtime.clock!.uptime >= originalUptime,
                  "UI coalescing suppressed raw callbacks or clock freshness")
        let raw = runtime.observation(device: "fixture-device")!
        try check(raw.sequence == 51 && raw.queueEpoch == 7 && raw.controlUptimeMs == UInt64(runtime.clock!.uptime * 1_000),
                  "Presentation retained an old control sequence, epoch or uptime")
        let command = RemoteOutputCommand(id: 1, generation: runtime.clockGeneration, sequence: 51,
            expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1_000) + 1_000,
            deviceId: "fixture-device", mediaId: media,
            control: .init(kind: .seek, positionMs: 4_500, percent: nil, enabled: nil, mode: nil, uri: nil))
        var receipt: Bool?
        runtime.send(command) { receipt = $0 }
        try await Task.sleep(for: .milliseconds(30))
        let writes = await owner.commands
        try check(writes == [1] && receipt == nil, "Latest coalesced sequence could not dispatch exactly one unacknowledged command")
        // A write is not a receipt; finish it with the real matching wire ACK.
        await owner.event("{\"event\":\"control_result\",\"id\":1,\"accepted\":true}")
        try await eventually("Coalesced transport receipt did not complete") { receipt == true }
        let completedWrites = await owner.commands
        try check(completedWrites == [1], "UI coalescing duplicated the transport write")

        for (sequence, position, state, rate, volume, title, next) in [
            (52, UInt64(4_500), "paused", 0.0, 0.4, "Original title", true),
            (53, UInt64(4_500), "playing", 1.0, 0.4, "Original title", true),
            (54, UInt64(4_500), "playing", 1.0, 0.7, "Original title", true),
            (55, UInt64(4_500), "playing", 1.0, 0.7, "Updated title", true),
            (56, UInt64(4_500), "playing", 1.0, 0.7, "Updated title", false)
        ] {
            let changes = ObservationCount()
            withObservationTracking { _ = runtime.clock } onChange: { changes.record() }
            await owner.event(try packet(UInt64(sequence), position: position, state: state, rate: rate, volume: volume, title: title, next: next))
            try await eventually("Meaningful clock update did not arrive") { runtime.clock?.sequence == UInt64(sequence) }
            try check(changes.count == 1, "Seek/state/volume/metadata/actions failed to invalidate clock presentation")
        }
        let cleared = ObservationCount(), beforeClear = callbacks
        withObservationTracking { _ = runtime.clock } onChange: { cleared.record() }
        await owner.event("{\"event\":\"clock\",\"clock\":null}")
        try await eventually("Clock gap did not clear raw ownership") { runtime.clock == nil }
        try check(cleared.count == 1 && callbacks == beforeClear + 1, "Clock gap did not invalidate presentation and notify transport")
        await runtime.shutdown()
    }
    @MainActor static func testLivePipeDelivery() async throws {
        let pipe = Pipe(), observation = PipeObservation()
        let reader = try SoloistOutputReader(handle: pipe.fileHandleForReading,receive: { observation.receive($0) },finished: { observation.finish() })
        defer { reader.cancel(); try? pipe.fileHandleForReading.close(); try? pipe.fileHandleForWriting.close() }
        let started = Data("{\"event\":\"started\",\"pid\":12345}\n".utf8)
        let began = ContinuousClock.now
        try pipe.fileHandleForWriting.write(contentsOf: started)
        try await eventually("A tiny startup event waited for pipe fill or EOF") { observation.snapshot().data == started }
        try check(ContinuousClock.now - began < .seconds(1) && observation.snapshot().finishes == 0, "Startup delivery must occur while the helper pipe remains open")
        let burst = Data(repeating: 97,count: 24_000)
        try pipe.fileHandleForWriting.write(contentsOf: burst)
        try await eventually("Pipe burst was truncated or stalled") { observation.snapshot().data.count == started.count + burst.count }
        let result = observation.snapshot()
        try check(result.data == started + burst && result.maximumChunk <= 4096, "Real pipe reads preserve byte order with bounded chunks")
        try pipe.fileHandleForWriting.close()
        try await eventually("Pipe EOF did not end the reader") { observation.snapshot().finishes == 1 }
        reader.cancel()

        let cancelled = Pipe(), ignored = PipeObservation()
        let stopped = try SoloistOutputReader(handle: cancelled.fileHandleForReading,receive: { ignored.receive($0) },finished: { ignored.finish() })
        stopped.cancel()
        try cancelled.fileHandleForWriting.write(contentsOf: started)
        try await Task.sleep(for: .milliseconds(100))
        try check(ignored.snapshot().data.isEmpty && ignored.snapshot().finishes == 0, "Intentional cancellation neither delivers queued bytes nor reports helper loss")
        try cancelled.fileHandleForReading.close(); try cancelled.fileHandleForWriting.close()
    }
    @MainActor static func testOutputClock() async throws {
        let credentials = MemoryCredentials(); let owner = PlaybackFixture()
        let runtime = SoloistRuntime(credentials: credentials, owner: owner, deviceName: "Auvra clock fixture")
        var callbacks = 0; runtime.observed = { callbacks += 1 }
        runtime.configure(key: "fixture-clock-key")
        try await eventually("Clock fixture did not start") { runtime.status.processID != nil }
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
        try await eventually("Clock fixture did not authenticate") { runtime.status.authenticated == true }
        let uri = "spotify:track:" + String(repeating: "a", count: 22)
        func packet(_ sequence: UInt64, state: String = "playing", rate: Double = 1.5, active: Bool = true, media: String? = nil, uptime: Double? = nil, extras: Bool = false) throws -> String {
            var clock: [String: Any] = ["sequence":sequence,"state":state,"rate":rate,"active":active,"media_id":media ?? uri,"position_ms":3500,"uptime":uptime ?? ProcessInfo.processInfo.systemUptime]
            if extras {
                clock["queue_epoch"] = 7
                clock["volume"] = 0.4; clock["options"] = ["shuffle":true,"repeat":"all","enhancement":"recommendations"]
                clock["actions"] = ["play":true,"pause":false,"seek":true,"next":false,"previous":false,"shuffle":true,"repeat":true,"queue_add":true,"seek_forward_ms":15000,"seek_backward_ms":30000]
                clock["metadata"] = ["title":"Original title", "artists":["Original artist"], "duration_ms":65000]
            }
            let value: [String: Any] = ["event":"clock","clock":clock]
            return String(data: try JSONSerialization.data(withJSONObject: value), encoding: .utf8)!
        }
        let first = Data((try packet(1,extras: true) + "\n").utf8), cut = first.count / 2
        await owner.bytes(first.prefix(cut)); await Task.yield()
        try check(runtime.clock == nil, "A partial clock frame escaped pipe framing")
        await owner.bytes(first.suffix(first.count - cut))
        try await eventually("A valid framed clock did not reach the host") { runtime.clock?.sequence == 1 }
        let observation = runtime.clock!.observation(device: "fixture-device", generation: runtime.clockGeneration)
        try check(observation.mediaId == uri && observation.rate == 1.5 && observation.positionMs >= 3500 && observation.positionMs < 4000, "Clock identity, rate or pipe-age compensation changed")
        try check(observation.volume == 0.4 && observation.options?.enhancement == .recommendations && observation.options?.repeat == .all && observation.actions?.queueAdd == true && observation.actions?.seekBackwardMs == 30000, "Clock options, volume or action whitelist did not survive the native host boundary")
        try check(observation.metadata?.title == "Original title" && observation.metadata?.durationMs == 65000, "Compact metadata did not reach the bridge observation")
        let expires = UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 1000
        let control = RemoteOutputCommand.Control(kind: .seek, positionMs: 4000, percent: nil, enabled: nil, mode: nil, uri: nil)
        let command = RemoteOutputCommand(id: 44, generation: runtime.clockGeneration, sequence: 1, expiresAtUptimeMs: expires,
                                          deviceId: "fixture-device", mediaId: uri, control: control)
        var receipt: Bool?
        runtime.send(command) { receipt = $0 }
        await owner.event("{\"event\":\"control_result\",\"id\":99,\"accepted\":true}")
        try await Task.sleep(for: .milliseconds(30))
        try check(receipt == nil && runtime.clock?.positionMs == 3500, "Unrelated result changed transport or completed the command")
        let commands = await owner.commands
        try check(commands == [1], "Bound local command did not reach the process owner exactly once")
        await owner.event("{\"event\":\"control_result\",\"id\":1,\"accepted\":true}")
        try await eventually("Owned control acknowledgement did not reach its request") { receipt == true }
        try check(runtime.clock?.positionMs == 3500, "A dispatch acknowledgement was mistaken for a playback observation")
        let entry: [String: Any] = ["media_id":uri, "source":"autoplay", "metadata":[
            "title":String(repeating: "Original queue title ", count: 13), "artists":["Original artist"], "duration_ms":0]]
        let queue: [String: Any] = ["epoch":7, "sequence":1, "media_id":uri, "uptime":ProcessInfo.processInfo.systemUptime,
                                    "previous":Array(repeating: entry, count: 10), "upcoming":Array(repeating: entry, count: 10)]
        var queueBytes = try JSONSerialization.data(withJSONObject: ["event":"queue", "queue":queue]); queueBytes.append(10)
        try check(queueBytes.count > 4096, "Queue regression must cross a pipe chunk boundary")
        for offset in stride(from: 0, to: queueBytes.count, by: 2048) {
            await owner.bytes(queueBytes[offset..<min(offset + 2048, queueBytes.count)])
        }
        try await eventually("Ordered pipe chunks did not project the queue") { runtime.queue?.upcoming.count == 10 }
        try check(runtime.queue?.upcoming.first?.source == .autoplay && runtime.clock?.sequence == 1, "Queue changed its source or the playback clock")
        try check(runtime.queue?.autoplayReady == nil, "A queue without confirmed readiness may not arm autoplay")
        let ready: [String: Any] = ["epoch":7, "sequence":2, "media_id":uri, "uptime":ProcessInfo.processInfo.systemUptime,
                                    "previous":[], "upcoming":[], "autoplay_ready":true]
        let readyBytes = try JSONSerialization.data(withJSONObject: ["event":"queue", "queue":ready])
        await owner.event(String(decoding: readyBytes, as: UTF8.self))
        try await eventually("Helper readiness did not survive the host queue boundary") { runtime.queue?.autoplayReady == true }
        let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
        let bridgeQueue = RemoteOutputQueue(generation: runtime.clockGeneration, epoch: 7, sequence: 2,
                                            deviceId: "fixture-device", mediaId: uri, previous: [], upcoming: [],
                                            autoplayReady: runtime.queue?.autoplayReady)
        let wire = try JSONSerialization.jsonObject(with: encoder.encode(bridgeQueue)) as? [String: Any]
        try check(wire?["autoplay_ready"] as? Bool == true, "The bridge dropped autoplay readiness")
        await owner.event(try packet(1, state: "paused", rate: 0)); await owner.event(try packet(2, rate: 9)); await owner.event(try packet(2, media: "not-a-media-id")); await owner.event(try packet(2, uptime: ProcessInfo.processInfo.systemUptime - 10))
        try await Task.sleep(for: .milliseconds(50))
        try check(runtime.clock?.sequence == 1 && runtime.clock?.state == .playing, "Invalid or stale clocks replaced a valid projection")
        let burst = Data((try packet(2) + "\n" + String(repeating: "{\"event\":\"ignored\"}\n", count: 260)).utf8)
        await owner.bytes(burst.prefix(50)); await owner.bytes(burst.dropFirst(50).prefix(4096)); await owner.bytes(burst.dropFirst(4146))
        try await eventually("A bounded burst of framed events was rejected") { runtime.clock?.sequence == 2 }
        try check(runtime.queue == nil, "Queue survived an incompatible clock epoch")
        try check(runtime.status.phase == .ready, "Valid coalesced helper frames caused a false protocol failure")
        await owner.event(try packet(3, state: "paused", rate: 0, active: false))
        try await eventually("Pause or device loss did not reach the host") { runtime.clock?.sequence == 3 && runtime.status.active == false }
        await owner.event("{\"event\":\"local_control_unavailable\"}")
        try await eventually("Lost socket retained its clock") { runtime.clock == nil }
        runtime.noteDeviceAvailable(true)
        try check(runtime.status.issue != nil, "A cached API device hid the missing local clock connection")
        await owner.event(try packet(4))
        try await eventually("Socket recovery did not clear its own issue") { runtime.clock?.sequence == 4 && runtime.status.issue == nil }
        runtime.retry()
        try await eventually("Clock fixture did not restart") { runtime.status.processID != nil && runtime.status.authenticated == nil }
        await owner.oldEvent(try packet(999))
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}"); await owner.event(try packet(1))
        try await eventually("New child generation did not accept a fresh sequence") { runtime.clock?.sequence == 1 }
        try check(runtime.clockGeneration > observation.generation, "Clock generation did not change with the owned process")
        await owner.event(try packet(2, uptime: ProcessInfo.processInfo.systemUptime - 5.8))
        try await eventually("A valid delayed packet was rejected") { runtime.clock?.sequence == 2 }
        try await Task.sleep(for: .milliseconds(350))
        try check(runtime.clock == nil && callbacks >= 4, "A silent helper retained an indefinitely fresh clock")
        await owner.event("{\"event\":\"output_closed\"}")
        try await eventually("A closed helper stream did not clear ownership") { runtime.status.phase == .failed && runtime.status.processID == nil && runtime.status.authenticated == nil }
        await owner.event("{\"event\":\"started\",\"pid\":99999}"); await owner.event("{\"event\":\"auth\",\"logged_in\":true}"); await owner.event(try packet(3))
        try await Task.sleep(for: .milliseconds(50))
        try check(runtime.status.phase == .failed && runtime.clock == nil, "Late stream callbacks restored ownership after EOF")
        await owner.event("{\"event\":\"exited\",\"code\":10}")
        try await eventually("Pipe EOF hid the real expired exit result") { runtime.status.phase == .expired }
        await runtime.shutdown()
    }
    @MainActor static func testURIStartClockGap() async throws {
        let original = "spotify:track:" + String(repeating: "a", count: 22)
        let target = "spotify:track:" + String(repeating: "b", count: 22)
        func clock(_ sequence: UInt64, active: Bool = true, media: String? = nil) throws -> String {
            let value: [String: Any] = ["event":"clock", "clock":[
                "sequence":sequence, "state":"playing", "rate":1, "active":active,
                "media_id":media ?? original, "position_ms":19_587, "uptime":ProcessInfo.processInfo.systemUptime,
                "queue_epoch":7,
            ]]
            return String(data: try JSONSerialization.data(withJSONObject: value), encoding: .utf8)!
        }
        func connected() async throws -> (SoloistRuntime, PlaybackFixture) {
            let owner = PlaybackFixture()
            let runtime = SoloistRuntime(credentials: MemoryCredentials(), owner: owner, deviceName: "Auvra URI-start fixture")
            runtime.configure(key: "fixture-uri-key")
            try await eventually("URI fixture did not start") { runtime.status.processID != nil }
            await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
            try await eventually("URI fixture did not authenticate") { runtime.status.authenticated == true }
            await owner.event(try clock(1))
            try await eventually("URI fixture did not receive its initial clock") { runtime.clock?.sequence == 1 }
            return (runtime, owner)
        }
        func command(_ runtime: SoloistRuntime, kind: RemoteOutputCommand.Control.Kind = .startUri) -> RemoteOutputCommand {
            RemoteOutputCommand(id: 44, generation: runtime.clockGeneration, sequence: 1,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 1000,
                deviceId: "fixture-device", mediaId: original,
                control: .init(kind: kind, positionMs: kind == .seek ? 100 : nil,
                    percent: nil, enabled: nil, mode: nil, uri: kind == .startUri ? target : nil))
        }
        func waitForWrite(_ owner: PlaybackFixture) async throws {
            let deadline = ContinuousClock.now + .seconds(2)
            while await owner.commands.isEmpty, ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            let writes = await owner.commands
            try check(writes == [1], "URI fixture did not invoke its single bounded write")
        }

        do {
            let (runtime, owner) = try await connected()
            await owner.delayNextWrite()
            var receipt: Bool?, completions = 0
            runtime.send(command(runtime)) { receipt = $0; completions += 1 }
            try await waitForWrite(owner)
            await owner.event("{\"event\":\"clock\",\"clock\":null}")
            try await eventually("URI gap retained public ownership") { runtime.clock == nil }
            try check(receipt == nil && runtime.observation(device: "fixture-device") == nil,
                      "An unidentified URI-start clock must clear ownership without deciding its receipt")
            await owner.event("{\"event\":\"control_result\",\"id\":99,\"accepted\":true}")
            await owner.finishDelayedWrite(true)
            try await Task.sleep(for: .milliseconds(30))
            try check(receipt == nil, "A write or unrelated ACK must not accept a pending URI start")
            await owner.event("{\"event\":\"clock\",\"clock\":null}")
            await owner.event(try clock(2, media: target))
            try await eventually("URI fixture did not project its later real clock") { runtime.clock?.sequence == 2 }
            try check(receipt == nil, "Actual Playing must not impersonate a matching wire ACK")
            await owner.event("{\"event\":\"control_result\",\"id\":1,\"accepted\":true}")
            try await eventually("The matching URI ACK was lost across the clock gap") { receipt == true }
            await owner.event("{\"event\":\"control_result\",\"id\":1,\"accepted\":false}")
            try await Task.sleep(for: .milliseconds(20))
            try check(completions == 1 && runtime.clock?.mediaId == target,
                      "Duplicate ACKs may neither finish twice nor manufacture another playback clock")
            await runtime.shutdown()
        }
        do {
            let (runtime, owner) = try await connected()
            await owner.delayNextWrite()
            var receipt: Bool?
            runtime.send(command(runtime)) { receipt = $0 }
            try await waitForWrite(owner)
            await owner.event("{\"event\":\"clock\",\"clock\":null}")
            try await eventually("Failed-write URI gap retained ownership") { runtime.clock == nil }
            await owner.finishDelayedWrite(false)
            try await eventually("A failed URI write survived its clock gap") { receipt == false }
            await owner.event("{\"event\":\"control_result\",\"id\":1,\"accepted\":true}")
            try await Task.sleep(for: .milliseconds(20))
            try check(receipt == false, "A late ACK resurrected a failed URI write")
            await runtime.shutdown()
        }
        for kind: RemoteOutputCommand.Control.Kind in [.play, .pause, .seek, .next, .previous] {
            let (runtime, owner) = try await connected()
            var receipt: Bool?
            runtime.send(command(runtime, kind: kind)) { receipt = $0 }
            try await waitForWrite(owner)
            await owner.event("{\"event\":\"clock\",\"clock\":null}")
            try await eventually("An ordinary control survived lost ownership: \(kind.rawValue)") { receipt == false && runtime.clock == nil }
            await runtime.shutdown()
        }
        for invalidation in ["auth", "inactive", "exit", "replacement", "retry", "malformed clock"] {
            let (runtime, owner) = try await connected()
            var receipt: Bool?
            runtime.send(command(runtime)) { receipt = $0 }
            try await waitForWrite(owner)
            await owner.event("{\"event\":\"clock\",\"clock\":null}")
            try await eventually("Invalidation fixture did not first clear its clock") { runtime.clock == nil }
            try check(receipt == nil, "A URI-start clock gap failed before the lifecycle boundary")
            switch invalidation {
            case "auth": await owner.event("{\"event\":\"auth\",\"logged_in\":false}")
            case "inactive": await owner.event(try clock(2, active: false))
            case "exit": await owner.event("{\"event\":\"output_closed\"}")
            case "replacement": await owner.event("{\"event\":\"started\",\"pid\":12346,\"clock_protocol\":1,\"control_protocol\":1}")
            case "retry": runtime.retry()
            default: await owner.event("{\"event\":\"clock\",\"clock\":\"invalid\"}")
            }
            try await eventually("A URI receipt survived its invalidated scope: \(invalidation)") { receipt == false }
            await owner.oldEvent("{\"event\":\"control_result\",\"id\":1,\"accepted\":true}")
            try await Task.sleep(for: .milliseconds(20))
            try check(receipt == false, "A late old-scope ACK revived invalidated URI start: \(invalidation)")
            await runtime.shutdown()
        }
        do {
            let (runtime, owner) = try await connected()
            var receipt: Bool?, completions = 0
            let began = ContinuousClock.now
            runtime.send(command(runtime)) { receipt = $0; completions += 1 }
            try await waitForWrite(owner)
            let gaps = Task {
                while !Task.isCancelled {
                    await owner.event("{\"event\":\"clock\",\"clock\":null}")
                    try? await Task.sleep(for: .milliseconds(200))
                }
            }
            try await eventually("An unacknowledged URI start outlived its original deadline") { receipt == false }
            gaps.cancel()
            try check(ContinuousClock.now - began < .milliseconds(3500), "Repeated clock gaps renewed the URI receipt deadline")
            await owner.event("{\"event\":\"control_result\",\"id\":1,\"accepted\":true}")
            try await Task.sleep(for: .milliseconds(20))
            try check(receipt == false && completions == 1 && runtime.clock == nil,
                      "A timeout or late ACK must neither duplicate completion nor reclaim ownership")
            await runtime.shutdown()
        }
    }
    @MainActor static func testSameMediaNext() async throws {
        let owner = PlaybackFixture()
        let runtime = SoloistRuntime(credentials: MemoryCredentials(), owner: owner, deviceName: "Auvra same-URI fixture")
        runtime.configure(key: "fixture-next-key")
        try await eventually("Same-URI fixture did not start") { runtime.status.processID != nil }
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
        try await eventually("Same-URI fixture did not authenticate") { runtime.status.authenticated == true }
        let device = "fixture-device"
        let item = "spotify:track:" + String(repeating: "a", count: 22)
        func clock(_ sequence: UInt64, epoch: UInt64, wireID: UInt64? = nil, media: String? = nil,
                   active: Bool = true) throws -> String {
            var data: [String: Any] = ["sequence":sequence, "state":"playing", "rate":1, "active":active,
                                       "media_id":media ?? item, "position_ms":10, "queue_epoch":epoch,
                                       "uptime":ProcessInfo.processInfo.systemUptime,
                                       "actions":["play":false,"pause":true,"seek":true,"next":true,"previous":true,
                                                  "shuffle":false,"repeat":false,"queue_add":false]]
            if let wireID { data["confirmed_same_media_next"] = wireID }
            let packet = try JSONSerialization.data(withJSONObject: ["event":"clock", "clock":data])
            return String(decoding: packet, as: UTF8.self)
        }
        func next(_ id: UInt64, sequence: UInt64) -> RemoteOutputCommand {
            RemoteOutputCommand(id: id, generation: runtime.clockGeneration, sequence: sequence,
                                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 1200,
                                deviceId: device, mediaId: item, control: .init(kind: .next, positionMs: nil,
                                    percent: nil, enabled: nil, mode: nil, uri: nil))
        }
        func acknowledgement(_ wireID: UInt64, accepted: Bool = true) -> String {
            "{\"event\":\"control_result\",\"id\":\(wireID),\"accepted\":\(accepted)}"
        }
        await owner.event(try clock(1, epoch: 7))
        try await eventually("Initial same-URI clock missing") { runtime.clock?.sequence == 1 }
        await owner.delayNextWrite()
        var receipt: Bool?
        runtime.send(next(44, sequence: 1)) { receipt = $0 }
        await owner.event(try clock(2, epoch: 8, wireID: 1))
        await owner.event(acknowledgement(99))
        try await Task.sleep(for: .milliseconds(30))
        try check(receipt == nil && runtime.clock?.sequence == 1, "The unidentified result published an exact-Navigation witness")
        await owner.event(try clock(3, epoch: 8))
        await owner.event(acknowledgement(1))
        try await eventually("The exact helper receipt did not complete Next") { receipt == true }
        try check(runtime.clock?.sequence == 1, "An acknowledged wire ID escaped before the owner reported the write")
        await owner.finishDelayedWrite(true)
        try await eventually("The held successor did not reach the host") { runtime.clock?.sequence == 3 }
        try check(runtime.observation(device: device)?.confirmedSameMediaNext == 44,
                  "The newer same-epoch clock must carry only the provider's command ID")
        try check(runtime.observation(device: "different-device")?.confirmedSameMediaNext == nil,
                  "The private helper witness cannot cross the selected output")

        receipt = nil
        runtime.send(next(45, sequence: 3)) { receipt = $0 }
        await owner.event(try clock(4, epoch: 9, wireID: 1))
        try await eventually("Stale witness hid ordinary clock projection") { runtime.clock?.sequence == 4 }
        try check(runtime.observation(device: device)?.confirmedSameMediaNext == nil,
                  "A previous wire ID completed a newer Next")
        await owner.event(try clock(5, epoch: 9, wireID: 2))
        await owner.event(acknowledgement(2, accepted: false))
        try await eventually("A rejected Next hid ordinary clock projection") { receipt == false && runtime.clock?.sequence == 5 }
        try check(runtime.observation(device: device)?.confirmedSameMediaNext == nil, "A rejected command marked the outgoing clock")

        receipt = nil
        runtime.send(next(46, sequence: 5)) { receipt = $0 }
        await owner.event(try clock(6, epoch: 10, active: false))
        await owner.event(try clock(7, epoch: 10, wireID: 3))
        await owner.event(acknowledgement(3))
        try await eventually("Inactive-clock sequence did not advance normally") { receipt == true && runtime.clock?.sequence == 7 }
        try check(runtime.observation(device: device)?.confirmedSameMediaNext == nil,
                  "A matching wire ID escaped across loss of the owned output")
        let sent = await owner.commands
        try check(sent == [1, 2, 3], "Each Next must reach the private pipe once")

        func strictCommand(_ epoch: UInt64?, kind: RemoteOutputCommand.Control.Kind = .pause,
                           media: String? = nil, generation: UInt64? = nil) -> RemoteOutputCommand {
            RemoteOutputCommand(id: 47, generation: generation ?? runtime.clockGeneration, sequence: 7,
                                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 1200,
                                requiredQueueEpoch: epoch, deviceId: device, mediaId: media ?? item,
                                control: .init(kind: kind, positionMs: nil, percent: nil, enabled: nil, mode: nil, uri: nil))
        }
        for (request, selectedDevice) in [
            (strictCommand(9), device), (strictCommand(0), device),
            (strictCommand(10, kind: .seek), device),
            (strictCommand(10, media: "spotify:track:" + String(repeating: "b", count: 22)), device),
            (strictCommand(10, generation: runtime.clockGeneration &+ 1), device),
            (strictCommand(10), "different-device"),
            (strictCommand(nil, kind: .previousFromStart), device),
            (strictCommand(9, kind: .previousFromStart), device),
            (strictCommand(10, kind: .previousFromStart), "different-device"),
            (strictCommand(9, kind: .next), device),
            (strictCommand(10, kind: .next, media: "spotify:track:" + String(repeating: "b", count: 22)), device),
            (strictCommand(10, kind: .next), "different-device"),
        ] {
            receipt = nil
            runtime.send(request, ownedDeviceID: selectedDevice) { receipt = $0 }
            try check(receipt == false, "An occurrence-scoped control accepted an invalid command or source binding")
        }
        receipt = nil
        runtime.send(strictCommand(10), ownedDeviceID: device) { receipt = $0 }
        for _ in 0..<50 {
            if await owner.commands.last == 4 { break }
            try await Task.sleep(for: .milliseconds(5))
        }
        let sentPause = await owner.commands
        try check(sentPause == [1, 2, 3, 4], "Only the matched strict Pause must reach the pipe")
        await owner.event(acknowledgement(4))
        try await eventually("The matched strict Pause did not receive its ACK") { receipt == true }

        receipt = nil
        runtime.send(strictCommand(10, kind: .previousFromStart), ownedDeviceID: device) { receipt = $0 }
        for _ in 0..<50 {
            if await owner.commands.last == 5 { break }
            try await Task.sleep(for: .milliseconds(5))
        }
        let sentPrevious = await owner.commands
        try check(sentPrevious == [1, 2, 3, 4, 5], "Only the matched compound Previous must reach the pipe")
        await owner.event(acknowledgement(5))
        try await eventually("The matched compound Previous did not receive its ACK") { receipt == true }

        receipt = nil
        runtime.send(strictCommand(10, kind: .next), ownedDeviceID: device) { receipt = $0 }
        for _ in 0..<50 {
            if await owner.commands.last == 6 { break }
            try await Task.sleep(for: .milliseconds(5))
        }
        let sentNext = await owner.commands
        try check(sentNext == [1, 2, 3, 4, 5, 6], "Only the matched occurrence-scoped Next must reach the pipe")
        await owner.event(acknowledgement(6))
        try await eventually("The matched occurrence-scoped Next did not receive its ACK") { receipt == true }
        await runtime.shutdown()
    }
    @MainActor static func testScopedRecoveryPlay() async throws {
        let owner = PlaybackFixture()
        let runtime = SoloistRuntime(credentials: MemoryCredentials(), owner: owner, deviceName: "Auvra recovery fixture")
        runtime.configure(key: "fixture-recovery-key")
        try await eventually("Recovery fixture did not start") { runtime.status.processID != nil }
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
        try await eventually("Recovery fixture did not authenticate") { runtime.status.authenticated == true }
        let device = "fixture-device"
        let media = "spotify:track:" + String(repeating: "a", count: 22)
        func observe(_ sequence: UInt64, state: String = "paused", position: UInt64 = 0,
                     playAllowed: Bool = true) async throws {
            let clock: [String: Any] = ["sequence":sequence, "state":state, "active":true,
                "media_id":media, "position_ms":position, "rate":state == "playing" ? 1 : 0,
                "uptime":ProcessInfo.processInfo.systemUptime, "queue_epoch":14,
                "actions":["play":playAllowed,"pause":true,"seek":true,"next":true,"previous":true,
                           "shuffle":false,"repeat":false,"queue_add":false]]
            let packet = try JSONSerialization.data(withJSONObject: ["event":"clock", "clock":clock])
            await owner.event(String(decoding: packet, as: UTF8.self))
            try await eventually("Recovery clock did not reach Runtime") { runtime.clock?.sequence == sequence }
        }
        func recovery(_ sequence: UInt64, epoch: UInt64 = 14) -> RemoteOutputCommand {
            RemoteOutputCommand(id: 120, generation: runtime.clockGeneration, sequence: sequence,
                expiresAtUptimeMs: UInt64(ProcessInfo.processInfo.systemUptime * 1000) + 1200,
                requestId: 24, requiredQueueEpoch: epoch, deviceId: device, mediaId: media,
                control: .init(kind: .play, positionMs: nil, percent: nil,
                    enabled: nil, mode: nil, uri: nil))
        }
        var outcome: Bool?
        func reject(_ command: RemoteOutputCommand, deviceID: String = "fixture-device") throws {
            outcome = nil
            runtime.send(command, ownedDeviceID: deviceID) { outcome = $0 }
            try check(outcome == false, "A recovery escaped the exact paused occurrence")
        }

        try await observe(1)
        try reject(recovery(1, epoch: 13))
        try reject(recovery(1), deviceID: "different-device")
        try await observe(2)
        final class PlaybackIntent { var current = true }
        let intent = PlaybackIntent()
        outcome = nil
        runtime.send(recovery(1), ownedDeviceID: device, admission: { intent.current }) { outcome = $0 }
        intent.current = false
        try await eventually("A newer intent did not fence the queued Play write") { outcome == false }
        let writesAfterNewIntent = await owner.commands
        try check(writesAfterNewIntent.isEmpty, "Recovery was written after its host intent was replaced")

        try await observe(3, state: "playing")
        try reject(recovery(3))
        try await observe(4, playAllowed: false)
        try reject(recovery(4))
        try await observe(5, position: 121)
        try reject(recovery(5))
        try await observe(6, position: 120)
        try reject(recovery(2))
        try reject(recovery(5))
        outcome = nil
        runtime.send(recovery(6), ownedDeviceID: device) { outcome = $0 }
        for _ in 0..<50 {
            let commands = await owner.commands
            if !commands.isEmpty { break }
            try await Task.sleep(for: .milliseconds(5))
        }
        let writes = await owner.commands
        try check(writes.count == 1 && outcome == nil, "Only the exact current pause should reach the wire, pending its receipt")
        await owner.event("{\"event\":\"control_result\",\"id\":\(writes[0]),\"accepted\":true}")
        try await eventually("The scoped Play did not receive its exact ACK") { outcome == true }
        await runtime.shutdown()
    }
    @MainActor static func testSelectionActivation() async throws {
        for scenario in ["ack", "nil", "wrong receipt", "cancel", "pre-cancel", "logout", "output closed",
                         "supervisor exit", "child exit", "retry", "shutdown", "write failed", "timeout", "already active", "continuous late ack"] {
            let owner = PlaybackFixture()
            let continuousClock = BootstrapFixtureClock()
            let runtime = SoloistRuntime(credentials: MemoryCredentials(), owner: owner, deviceName: "Original bootstrap fixture",
                                         activationContinuousTime: { continuousClock.read() })
            runtime.configure(key: "original-bootstrap-key")
            try await eventually("Bootstrap fixture did not launch") { runtime.status.processID != nil }
            try check(!runtime.canActivateForSelection, "An unauthenticated launch cannot bootstrap")
            await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
            try await eventually("An authenticated inactive launch must allow explicit activation") { runtime.canActivateForSelection }
            if scenario == "already active" {
                let now = ProcessInfo.processInfo.systemUptime
                await owner.event("{\"event\":\"clock\",\"clock\":{\"sequence\":1,\"state\":\"idle\",\"position_ms\":0,\"rate\":0,\"active\":true,\"uptime\":\(now)}}")
                try await eventually("The real active observation did not arrive") { runtime.status.active == true }
                let result = await runtime.activateForSelection()
                let calls = await owner.activations
                try check(!result && calls.isEmpty, "An already active output cannot acquire another bootstrap write")
                await runtime.shutdown(); continue
            }
            if scenario == "write failed" { await owner.rejectActivationWrite() }
            var receipt: Bool?
            let request = Task { let result = await runtime.activateForSelection(); receipt = result; return result }
            if scenario == "pre-cancel" {
                request.cancel()
                let result = await request.value
                let calls = await owner.activations
                try check(!result && calls.isEmpty, "Pre-cancelled activation must never reach the owner")
                await runtime.shutdown(); continue
            }
            for _ in 0..<100 {
                if !(await owner.activations).isEmpty { break }
                try await Task.sleep(for: .milliseconds(5))
            }
            let calls = await owner.activations
            try check(calls.count == 1, "Bootstrap must write once: \(scenario)")
            let call = calls[0]
            let nowMs = UInt64(ProcessInfo.processInfo.systemUptime * 1000)
            let continuous = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
            try check(call.generation == runtime.clockGeneration && call.deadline >= nowMs && call.deadline - nowMs <= 1_500,
                      "Bootstrap must retain its original monotonic dispatch deadline and runtime")
            try check(call.continuousDeadline >= continuous && call.continuousDeadline - continuous <= 1_500_000_000,
                      "Bootstrap must also expire while the machine sleeps")
            if !["write failed"].contains(scenario) {
                try check(!runtime.canActivateForSelection, "Bootstrap must hold the shared native wire credit")
                let duplicate = await runtime.activateForSelection()
                try check(!duplicate, "A second activation cannot overlap the first")
            }
            switch scenario {
            case "nil":
                await owner.event("{\"event\":\"clock\",\"clock\":null}")
                try await Task.sleep(for: .milliseconds(20))
                try check(receipt == nil, "An absent media clock cannot cancel an authenticated inactive bootstrap")
            case "wrong receipt":
                await owner.event("{\"event\":\"control_result\",\"id\":\(call.id),\"accepted\":true}")
                await owner.event("{\"event\":\"activation_result\",\"id\":\(call.id + 1),\"accepted\":true}")
                try await Task.sleep(for: .milliseconds(20))
                try check(receipt == nil, "A transport or another bootstrap receipt cannot acknowledge activation")
            case "cancel": request.cancel()
            case "logout": await owner.event("{\"event\":\"auth\",\"logged_in\":false}")
            case "output closed": await owner.event("{\"event\":\"output_closed\"}")
            case "supervisor exit": await owner.event("{\"event\":\"supervisor_exited\",\"code\":10}")
            case "child exit": await owner.event("{\"event\":\"exited\",\"code\":10}")
            case "retry": runtime.retry()
            case "shutdown": await runtime.shutdown()
            case "continuous late ack":
                // Simulate waking before the scheduled timeout callback runs:
                // awake time barely moved, but this receipt is already 3s late.
                continuousClock.advance(3_000_000_000)
                await owner.event("{\"event\":\"activation_result\",\"id\":\(call.id),\"accepted\":true}")
            default: break
            }
            let accepts = ["ack", "nil", "wrong receipt"].contains(scenario)
            if accepts { await owner.event("{\"event\":\"activation_result\",\"id\":\(call.id),\"accepted\":true}") }
            let result = await request.value
            try check(result == accepts, "Activation receipt outcome crossed a lifecycle scope: \(scenario)")
            let after = await owner.activations
            try check(after.count == 1, "Cancellation or timeout retried activation")
            if accepts {
                try check(runtime.clock == nil && runtime.status.active == false,
                          "An activation ACK cannot fabricate active media, position or playback rights")
            } else {
                await owner.event("{\"event\":\"activation_result\",\"id\":\(call.id),\"accepted\":true}")
                try await Task.sleep(for: .milliseconds(20))
                try check(receipt == false, "A late receipt revived a cancelled activation")
            }
            await runtime.shutdown()
        }
        print("Selection activation runtime fixtures passed: 15 launch/auth/credit/cancel/deadline/receipt boundaries, including late continuous ACK; no media ownership.")
    }
    @MainActor static func testPlaybackLifecycle() async throws {
        let credentials = MemoryCredentials(); let owner = PlaybackFixture()
        let runtime = SoloistRuntime(credentials: credentials, owner: owner, deviceName: "Auvra test device")
        runtime.restore()
        try await eventually("Empty Keychain must leave playback unconfigured") { runtime.status.phase == .notConfigured }
        let emptyStarts = await owner.starts
        try check(emptyStarts == 0, "A missing key launched playback")
        runtime.configure(key: "fixture-soloist-key")
        try await eventually("Playback did not start") { runtime.status.processID == 12345 && runtime.status.phase == .pairing }
        runtime.noteDeviceAvailable(true)
        try check(runtime.status.phase == .ready, "Owned Connect device was not accepted")
        await owner.event("{\"event\":\"auth\",\"logged_in\":false}")
        try await eventually("Local logout did not clear ready") { runtime.status.phase == .pairing }
        runtime.noteDeviceAvailable(true)
        try check(runtime.status.phase == .pairing, "A stale API device overrode native logout")
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
        try await eventually("Restored native login did not become ready") { runtime.status.phase == .ready }
        runtime.noteDeviceAvailable(false)
        try check(runtime.status.phase == .ready, "An inactive API device erased the restored native login")
        await owner.event("{\"event\":\"exited\",\"code\":10}")
        try await eventually("Expired runtime must require update") { runtime.status.phase == .expired && runtime.status.processID == nil }
        await owner.event("{\"event\":\"started\",\"pid\":99999}")
        try await Task.sleep(for: .milliseconds(2200))
        let expiredStarts = await owner.starts
        try check(expiredStarts == 1 && runtime.status.processID == nil && runtime.status.phase == .expired, "Expired runtime retried or accepted a late started event")
        runtime.retry()
        try await eventually("Retry did not restore playback") { runtime.status.processID == 12345 }
        await runtime.shutdown()
        try check(runtime.status.processID == nil, "Shutdown retained process ownership")
        runtime.restore()
        try await eventually("Saved key did not restore playback") { runtime.status.processID == 12345 }
        await runtime.forgetKey()
        let remaining = await credentials.saved
        try check(remaining == nil, "Disconnect retained the Soloist key")
        let delayed = DelayedPlaybackCredentials(); let lateOwner = PlaybackFixture()
        let closing = SoloistRuntime(credentials: delayed, owner: lateOwner, deviceName: "Auvra delayed fixture")
        closing.restore()
        for _ in 0..<100 { if await delayed.hasPendingRead() { break }; try await Task.sleep(for: .milliseconds(10)) }
        await closing.shutdown()
        await delayed.finish()
        try await Task.sleep(for: .milliseconds(100))
        let lateStarts = await lateOwner.starts
        try check(lateStarts == 0 && closing.status.processID == nil, "A late Keychain reply launched playback after Quit")
    }
    static func testRuntimeInstaller() async throws {
        let manager = FileManager.default
        let root = manager.temporaryDirectory.appendingPathComponent("auvra-install-" + UUID().uuidString, isDirectory: true)
        try manager.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        defer { try? manager.removeItem(at: root) }
        let directory = root.appendingPathComponent("PlaybackRuntime", isDirectory: true)
        let destination = directory.appendingPathComponent("soloist")
        let installer = SoloistRuntimeInstaller(directory: directory)
        let invalid = root.appendingPathComponent("invalid")
        try Data("not an executable".utf8).write(to: invalid)
        let alias = root.appendingPathComponent("symlink")
        try manager.createSymbolicLink(at: alias, withDestinationURL: URL(fileURLWithPath: "/usr/bin/true"))
        let huge = root.appendingPathComponent("oversized")
        try Data().write(to: huge)
        let hugeFile = try FileHandle(forWritingTo: huge)
        try hugeFile.truncate(atOffset: 128 * 1024 * 1024 + 1)
        try hugeFile.close()
        try manager.createDirectory(at: directory, withIntermediateDirectories: false)
        let original = Data("previous installation".utf8)
        try original.write(to: destination)
        for source in [invalid, alias, huge, root, URL(fileURLWithPath: "/usr/bin/true")] {
            var rejected = false
            do { try await installer.install(source) } catch { rejected = true }
            try check(rejected, "An unsupported file was installed")
            try check(Data(contentsOf: destination) == original, "A failed import replaced the existing runtime")
            try check(manager.contentsOfDirectory(atPath: directory.path) == ["soloist"], "A failed import retained staged data")
        }
        if let path = ProcessInfo.processInfo.environment["AUVRA_SOLOIST_TEST_EXECUTABLE"] {
            let downloaded = root.appendingPathComponent("downloaded-soloist")
            try manager.copyItem(at: URL(fileURLWithPath: path), to: downloaded)
            try manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: downloaded.path)
            let quarantine = Data("0081;00000000;AuvraFixture;".utf8)
            let name = "com.apple.quarantine"
            try check(quarantine.withUnsafeBytes { setxattr(downloaded.path, name, $0.baseAddress, $0.count, 0, 0) } == 0, "Could not set fixture quarantine")
            try await installer.install(downloaded)
            try SoloistRuntimeInstaller.validate(destination)
            try check(access(destination.path, X_OK) == 0 && manager.contentsEqual(atPath: downloaded.path, andPath: destination.path),
                      "The installed runtime is not the selected executable")
            var installedQuarantine = Data(count: 4096)
            let length = installedQuarantine.withUnsafeMutableBytes { getxattr(destination.path, name, $0.baseAddress, $0.count, 0, 0) }
            // macOS can restamp the origin metadata when copying by descriptor.
            let flags = length > 0 ? String(decoding: installedQuarantine.prefix(length), as: UTF8.self).split(separator: ";").first : nil
            try check(flags.flatMap { UInt16($0, radix: 16) }.map { $0 & 0x81 == 0x81 } == true,
                      "The selected file’s quarantine flags were not preserved")
            try check(manager.contentsOfDirectory(atPath: directory.path) == ["soloist"], "Successful import retained staged data")
            print("Signed Apple Silicon runtime copy and quarantine verified in an isolated directory.")
        }
        print("Installer rejects unsupported files and preserves the existing executable.")
    }
    @MainActor static func testInstallationLifecycle() async throws {
        let credentials = MemoryCredentials(), owner = PlaybackFixture(), installer = InstallerFixture()
        let runtime = SoloistRuntime(credentials: credentials, owner: owner, deviceName: "Auvra install fixture", installer: installer)
        let valid = URL(fileURLWithPath: "/original-fixture/soloist")
        runtime.installRuntime(from: valid)
        runtime.installRuntime(from: valid)
        try await eventually("An installation without a key should finish without launch") {
            !runtime.isInstalling && runtime.installNotice != nil && runtime.status.phase == .notConfigured
        }
        let firstAttempts = await installer.attempts
        let firstStarts = await owner.starts
        try check(firstAttempts == 1 && firstStarts == 0, "Duplicate installs or a missing key launched playback")
        runtime.configure(key: "fixture-soloist-key")
        try await eventually("Playback setup did not launch") { runtime.status.processID != nil }
        await owner.event("{\"event\":\"auth\",\"logged_in\":true}")
        try await eventually("Playback did not become ready") { runtime.status.phase == .ready }
        runtime.installRuntime(from: URL(fileURLWithPath: "/original-fixture/invalid"))
        try await eventually("A rejected install did not produce feedback") { !runtime.isInstalling && runtime.installIssue != nil }
        try check(runtime.status.phase == .ready && runtime.status.processID != nil, "A failed update stopped working playback")
        runtime.installRuntime(from: valid)
        try await eventually("A successful update did not offer reconnect") { !runtime.isInstalling && runtime.installNotice?.contains("Reconnect") == true }
        let beforeReconnect = await owner.starts
        try check(beforeReconnect == 1 && runtime.status.phase == .ready, "Installing an update restarted the running engine")
        runtime.retry()
        try await eventually("Explicit reconnect did not apply the update") { runtime.status.processID != nil && runtime.status.phase == .pairing }
        await owner.event("{\"event\":\"exited\",\"code\":10}")
        try await eventually("Fixture did not reach expiry") { runtime.status.phase == .expired }
        runtime.installRuntime(from: valid)
        try await eventually("Updating an expired engine did not reuse the saved key") { runtime.status.processID != nil && runtime.status.phase == .pairing }
        runtime.installRuntime(from: valid)
        await runtime.shutdown()
        try await Task.sleep(for: .milliseconds(60))
        let afterQuit = await owner.starts
        try check(afterQuit == 3 && !runtime.isInstalling && runtime.installNotice == nil && runtime.status.processID == nil,
                  "A late installer result changed playback state after Quit")
    }
    static func testPKCE() throws {
        let pkce = SpotifyPKCE(verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", state: "fixture-state")
        try check(pkce.challenge == "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", "RFC 7636 S256 vector")
        let a = try SpotifyPKCE(), b = try SpotifyPKCE()
        try check(a.verifier.count == 86 && a.state.count == 86 && a.verifier != b.verifier && a.state != a.verifier, "Independent cryptographic PKCE values")
        let url = pkce.authorizationURL(clientID: String(repeating: "a", count: 32), redirect: URL(string: SpotifyOAuthClient.redirectURI)!)
        let query = parameters(URLComponents(url: url, resolvingAgainstBaseURL: false)!.percentEncodedQuery!)
        try check(url.host == "accounts.spotify.com" && query["code_challenge_method"] == "S256" && query["client_secret"] == nil, "Public client authorization request")
        try check(query["scope"] == SpotifyOAuthClient.scopes.joined(separator: " "), "Intended permissions in the authorization URL")
        try check(query["scope"]?.split(separator: " ").contains("user-read-playback-position") == true,
                  "Interactive consent requests saved podcast and audiobook progress")
        try check(try SpotifyPKCE.callbackCode("/spotify/callback?state=fixture-state&code=fixture-code", expectedState: "fixture-state") == "fixture-code", "Valid callback")
        for target in ["/wrong?state=fixture-state&code=x", "/spotify/callback?state=wrong&code=x", "/spotify/callback?state=fixture-state&state=fixture-state&code=x", "/spotify/callback?state=fixture-state&code=x&code=y", "/spotify/callback?state=fixture-state&error=access_denied"] {
            do { _ = try SpotifyPKCE.callbackCode(target, expectedState: "fixture-state"); throw NSError(domain: "Callback unexpectedly accepted", code: 1) }
            catch is SpotifyLoginFailure {}
        }
    }
    static func testLoopback() async throws {
        let listener = LoopbackOAuthCallback(state: "fixture-state")
        let redirect = try await listener.start()
        let collision = LoopbackOAuthCallback(state: "other", port: UInt16(redirect.port!))
        do { _ = try await collision.start(); throw NSError(domain: "Port collision accepted", code: 1) } catch SpotifyLoginFailure.callbackUnavailable {}
        var components = URLComponents(url: redirect, resolvingAgainstBaseURL: false)!
        components.queryItems = [.init(name: "state", value: "wrong"), .init(name: "code", value: "fixture-code")]
        let (_, bad) = try await URLSession.shared.data(from: components.url!)
        try check((bad as? HTTPURLResponse)?.statusCode == 400, "Invalid state rejected without consuming login")
        components.queryItems = [.init(name: "state", value: "fixture-state"), .init(name: "code", value: "fixture-code")]
        let (body, response) = try await URLSession.shared.data(from: components.url!)
        try check((response as? HTTPURLResponse)?.statusCode == 200, "Loopback callback received")
        let code = try await listener.code()
        try check(code == "fixture-code" && !String(decoding: body, as: UTF8.self).contains(code), "Callback never echoes credential")
        let cancelled = LoopbackOAuthCallback(state: "cancel")
        _ = try await cancelled.start(); cancelled.cancel(); cancelled.cancel()
        do { _ = try await cancelled.code(); throw NSError(domain: "Cancelled callback accepted", code: 1) } catch SpotifyLoginFailure.cancelled {}
        let timedOut = LoopbackOAuthCallback(state: "timeout", timeout: 0.05)
        _ = try await timedOut.start()
        do { _ = try await timedOut.code(); throw NSError(domain: "Callback never timed out", code: 1) } catch SpotifyLoginFailure.timedOut {}
        // The same explicit port is available again after cancellation of its previous listener.
        let fixed = LoopbackOAuthCallback(state: "fixed", port: UInt16(redirect.port!))
        _ = try await fixed.start(); fixed.cancel()
    }
    static func testTokenFailures() async throws {
        let client = oauthClient()
        TokenFixtures.shared.reset([(400, "{\"error\":\"invalid_grant\"}")])
        do { _ = try await client.refresh(clientID: "fixture", refreshToken: "revoked"); throw NSError(domain: "Revocation accepted", code: 1) } catch SpotifyLoginFailure.invalidGrant {}
        TokenFixtures.shared.reset([(200, "{\"access_token\":\"fixture\",\"token_type\":\"Bearer\",\"expires_in\":3600,\"scope\":\"user-library-read\"}")])
        do { _ = try await client.refresh(clientID: "fixture", refreshToken: "fixture"); throw NSError(domain: "Missing permissions accepted", code: 1) } catch SpotifyLoginFailure.permissions {}
        TokenFixtures.shared.reset([(200, token(scope: legacyPlaybackScopes))])
        let oldGrant = try await client.refresh(clientID: "fixture", refreshToken: "fixture")
        try check(!oldGrant.accessToken.isEmpty && oldGrant.scope == legacyPlaybackScopes,
                  "Existing playback authorization refreshes without the optional progress permission")
        TokenFixtures.shared.reset([(302, "{}")])
        do { _ = try await client.refresh(clientID: "fixture", refreshToken: "fixture"); throw NSError(domain: "Redirect accepted", code: 1) } catch SpotifyLoginFailure.authorization {}
    }
    @MainActor static func testAuthorization() async throws {
        let credentials = MemoryCredentials(), observation = Observation()
        TokenFixtures.shared.reset([
            (200, token()),
            (200, token("fixture-access-2", refresh: nil, scope: nil)),
            (200, token("fixture-access-3", refresh: "rotated-refresh", scope: legacyPlaybackScopes)),
            (200, token("fixture-access-4", refresh: nil, scope: nil)),
            (400, "{\"error\":\"invalid_grant\"}")
        ])
        let auth = SpotifyAuthorization(credentials: credentials, client: oauthClient(), status: { observation.status = $0 }, access: { observation.tokens.append($0) }, disconnected: { observation.disconnected += 1 }, openBrowser: { url in
            observation.authorizationURL = url
            let values = parameters(URLComponents(url: url, resolvingAgainstBaseURL: false)!.percentEncodedQuery!)
            var redirect = URLComponents(string: values["redirect_uri"]!)!
            redirect.queryItems = [.init(name: "state", value: values["state"]), .init(name: "code", value: "fixture-code")]
            let callback = redirect.url!
            Task { do { _ = try await URLSession.shared.data(from: callback) } catch { observation.callbackError = error } }
            return true
        }, callbackPort: nil)
        auth.connect(clientID: String(repeating: "a", count: 32))
        let initialDisconnects = observation.disconnected
        try await eventually("OAuth exchange delivered access", { observation.tokens.count == 1 || observation.status.issue != nil })
        try check(observation.tokens == ["fixture-access"] && observation.callbackError == nil, "Full PKCE/loopback/token flow")
        try check(observation.status.spokenProgressAccess == .granted, "Explicit progress scope is projected separately from login")
        let stored = await credentials.read(account: "spotify")!
        let storedFields = try JSONSerialization.jsonObject(with: stored) as! [String: Any]
        try check(Set(storedFields.keys) == ["clientID", "refreshToken", "spokenProgressGranted"]
                  && storedFields["spokenProgressGranted"] as? Bool == true,
                  "Secure storage contains the refresh credential and only one permission flag")
        let request = TokenFixtures.shared.recorded()[0]
        let form = parameters(String(decoding: request.httpBody ?? Data(), as: UTF8.self))
        let authorize = parameters(URLComponents(url: observation.authorizationURL!, resolvingAgainstBaseURL: false)!.percentEncodedQuery!)
        let proof = SpotifyPKCE(verifier: form["code_verifier"] ?? "", state: "")
        try check(proof.challenge == authorize["code_challenge"] && form["code"] == "fixture-code" && form["client_secret"] == nil && request.httpMethod == "POST", "Exchange uses original verifier and exact redirect")
        try check(authorize["show_dialog"] == nil && form["show_dialog"] == nil, "Ordinary sign-in uses Spotify's default consent behavior")
        auth.unauthorized()
        try await eventually("Refresh delivered access", { observation.tokens.count == 2 })
        let unchanged = await credentials.read(account: "spotify")!
        let unchangedFields = try JSONSerialization.jsonObject(with: unchanged) as! [String: Any]
        try check(unchangedFields["refreshToken"] as? String == "fixture-refresh"
                  && unchangedFields["clientID"] as? String == String(repeating: "a", count: 32)
                  && observation.status.spokenProgressAccess == .granted,
                  "Refresh without a new token or scope retains the matching saved grant")
        auth.noteConnected(); auth.unauthorized()
        try await eventually("Rotated refresh delivered access", { observation.tokens.count == 3 })
        let rotated = await credentials.read(account: "spotify")!
        try check(String(decoding: rotated, as: UTF8.self).contains("rotated-refresh"), "Refresh rotation saved")
        try check(observation.status.phase == .signedIn && observation.status.spokenProgressAccess == .notGranted,
                  "Explicitly narrower scopes replace the old flag without disconnecting playback")
        auth.noteConnected(); auth.unauthorized()
        try await eventually("Refresh with omitted narrower scope delivered access", { observation.tokens.count == 4 })
        try check(observation.status.spokenProgressAccess == .notGranted,
                  "Refresh with no scope cannot regrant a previously omitted optional permission")
        auth.noteConnected(); auth.unauthorized()
        try await eventually("Revocation clears account", { observation.status.issue != nil && observation.disconnected == initialDisconnects + 1 })
        let revoked = await credentials.read(account: "spotify")
        try check(revoked == nil && observation.tokens.count == 4 && observation.status.spokenProgressAccess == .unknown,
                  "Invalid grant deletes saved login and clears permission evidence")
        auth.connect(clientID: String(repeating: "a", count: 32)); auth.cancel()
        try await Task.sleep(for: .milliseconds(100))
        try check(observation.tokens.count == 4 && observation.status.phase == .disconnected
                  && observation.status.spokenProgressAccess == .unknown, "Cancelled generation cannot restore login or scope evidence")
        await credentials.write(stored, account: "spotify")
        let beforeStaleUnauthorized = TokenFixtures.shared.recorded().count
        auth.unauthorized()
        try await Task.sleep(for: .milliseconds(100))
        try check(TokenFixtures.shared.recorded().count == beforeStaleUnauthorized && observation.status.phase == .disconnected, "Late unauthorized update cannot reconnect after cancellation")
        auth.disconnect()
        try await eventually("Disconnect finished", { observation.status.phase == .disconnected })
        let deleted = await credentials.read(account: "spotify")
        try check(deleted == nil, "Disconnect deletes persisted login")
        auth.shutdown()
    }
    @MainActor static func testPermissionUpdate() async throws {
        let credentials = MemoryCredentials(), observation = Observation()
        let clientID = String(repeating: "c", count: 32)
        let priorScopes = SpotifyOAuthClient.scopes.filter { $0 != SpotifyOAuthClient.spokenProgressScope }.joined(separator: " ")
        let saved = try JSONSerialization.data(withJSONObject: ["clientID":clientID,"refreshToken":"prior-refresh","spokenProgressGranted":false])
        await credentials.write(saved, account: "spotify")
        TokenFixtures.shared.resetReplies([
            .http(200, token("prior-access", refresh: nil, scope: priorScopes)),
            .failure(.notConnectedToInternet),
            .http(200, token("updated-access", refresh: "updated-refresh")),
            .http(200, token("renewed-access", refresh: nil, scope: nil))
        ])
        let auth = SpotifyAuthorization(credentials: credentials, client: oauthClient(), status: { observation.status = $0 },
            access: { observation.tokens.append($0) }, disconnected: { observation.disconnected += 1 },
            openBrowser: { observation.authorizationURL = $0; return true }, callbackPort: nil)
        defer { auth.shutdown() }
        auth.restore(clientID: clientID)
        try await eventually("Prior authorization did not restore") { observation.tokens == ["prior-access"] }
        let priorCredential = await credentials.read(account: "spotify")

        func beginUpdate() async throws -> URLComponents {
            let priorGrant = observation.status.spokenProgressAccess
            observation.authorizationURL = nil
            auth.updatePermissions()
            try check(observation.status.phase == .signedIn && observation.status.permissionUpdate == .requesting
                      && observation.status.spokenProgressAccess == priorGrant, "Interactive update retains the signed-in state and existing grant")
            try await eventually("Permission update did not start the loopback listener") { observation.authorizationURL != nil }
            let authorize = parameters(URLComponents(url: observation.authorizationURL!, resolvingAgainstBaseURL: false)!.percentEncodedQuery!)
            try check(authorize["client_id"] == clientID && authorize["scope"]?.split(separator: " ").contains("user-read-playback-position") == true,
                      "Permission update requests consent using the established app and progress scope")
            try check(authorize["show_dialog"] == "true" && authorize["response_type"] == "code" && authorize["code_challenge_method"] == "S256",
                      "Explicit permission update requests a fresh consent screen using the same PKCE flow")
            var redirect = URLComponents(string: authorize["redirect_uri"]!)!
            redirect.queryItems = [.init(name: "state", value: authorize["state"])]
            return redirect
        }
        func checkRetained(_ action: String, requests: Int) async throws {
            let retained = await credentials.read(account: "spotify")
            try check(observation.status.phase == .signedIn && observation.status.permissionUpdate == nil
                      && observation.status.spokenProgressAccess == .notGranted && retained == priorCredential
                      && observation.tokens == ["prior-access"] && observation.disconnected == 0
                      && TokenFixtures.shared.recorded().count == requests,
                      "\(action) must retain the existing provider access, scope and saved login")
        }
        func respond(_ redirect: URLComponents, _ reply: URLQueryItem) async throws {
            var callback = redirect; callback.queryItems?.append(reply)
            _ = try await URLSession.shared.data(from: callback.url!)
        }

        _ = try await beginUpdate()
        auth.cancel(); auth.cancel()
        try await checkRetained("Cancel", requests: 1)

        let denial = try await beginUpdate()
        try await respond(denial, .init(name: "error", value: "access_denied"))
        try await eventually("Permission denial did not leave the update") { observation.status.issue != nil }
        try await checkRetained("Denied consent", requests: 1)

        let offline = try await beginUpdate()
        try await respond(offline, .init(name: "code", value: "fixture-unreachable-exchange"))
        try await eventually("Network failure did not leave the update") { observation.status.issue != nil }
        try await checkRetained("Failed code exchange", requests: 2)

        let success = try await beginUpdate()
        try await respond(success, .init(name: "code", value: "fixture-updated-grant"))
        try await eventually("Accepted permissions did not replace the authorization") { observation.tokens.count == 2 }
        let updated = await credentials.read(account: "spotify")!
        let fields = try JSONSerialization.jsonObject(with: updated) as! [String: Any]
        try check(observation.tokens == ["prior-access", "updated-access"] && observation.disconnected == 1
                  && observation.status.phase == .signedIn && observation.status.permissionUpdate == nil
                  && observation.status.spokenProgressAccess == .granted && fields["refreshToken"] as? String == "updated-refresh",
                  "Successful consent commits the new refresh token and replaces provider access once")

        _ = try await beginUpdate()
        auth.unauthorized()
        try await eventually("Provider recovery did not preempt an unfinished permission update") { observation.tokens.count == 3 }
        try check(observation.tokens.last == "renewed-access" && observation.disconnected == 1
                  && observation.status.phase == .signedIn && observation.status.permissionUpdate == nil
                  && observation.status.spokenProgressAccess == .granted,
                  "Normal bearer recovery keeps the last committed grant when consent has not completed")
        let recovery = TokenFixtures.shared.recorded().last!
        let form = parameters(String(decoding: recovery.httpBody ?? Data(), as: UTF8.self))
        try check(form["grant_type"] == "refresh_token" && form["refresh_token"] == "updated-refresh" && form["show_dialog"] == nil,
                  "Bearer recovery uses only the last committed refresh credential without requesting consent")
    }
    @MainActor static func testRefreshRecovery() async throws {
        let clientID = String(repeating: "b",count: 32)
        let saved = try JSONSerialization.data(withJSONObject: ["clientID":clientID,"refreshToken":"recovery-refresh"])
        func make(_ credentials: any CredentialStore, _ observation: Observation, delays: [TimeInterval] = [0.01,0.01,0.01]) -> SpotifyAuthorization {
            SpotifyAuthorization(credentials: credentials,client: oauthClient(),status: { observation.status = $0 },access: { observation.tokens.append($0) },disconnected: { observation.disconnected += 1 },openBrowser: { url in observation.authorizationURL = url; return false },callbackPort: nil,retryDelays: delays)
        }
        do {
            let credentials = MemoryCredentials(), observation = Observation()
            await credentials.write(saved,account: "spotify")
            let basicReply = token("recovered-access", refresh: nil, scope: legacyPlaybackScopes)
            TokenFixtures.shared.resetReplies([.failure(.notConnectedToInternet),.http(503,"{}"),.http(200,basicReply)])
            let auth = make(credentials,observation); auth.restore(clientID: clientID)
            try await eventually("A temporary network/service failure did not recover") { observation.status.phase == .signedIn }
            let requests = TokenFixtures.shared.recorded()
            try check(observation.tokens == ["recovered-access"] && observation.authorizationURL == nil && requests.count == 3
                      && observation.disconnected == 0, "Temporary failures must retain authorization and recover without browser consent")
            try check(requests.allSatisfy { let body = parameters(String(decoding: $0.httpBody ?? Data(),as: UTF8.self)); return body["grant_type"] == "refresh_token" && body["scope"] == nil && body["refresh_token"] == "recovery-refresh" }, "Refresh retries changed their grant, permissions or credential")
            try check(!observation.status.canRetrySavedLogin && observation.status.issue == nil
                      && observation.status.spokenProgressAccess == .notGranted, "Recovery projects legacy scope without stale retry state")
            auth.shutdown()
        }
        do {
            let credentials = MemoryCredentials(), observation = Observation()
            await credentials.write(saved,account: "spotify")
            let failures: [TokenFixtures.Reply] = [.failure(.timedOut), .http(503,"{}"), .failure(.notConnectedToInternet),
                                                    .failure(.networkConnectionLost), .http(502,"{}"), .failure(.timedOut)]
            TokenFixtures.shared.resetReplies(failures + [.http(200,token("later-access",refresh: nil,scope: nil))])
            let auth = make(credentials,observation,delays: [0.04,0.08]); auth.restore(clientID: clientID)
            try await eventually("Refresh did not continue past the former retry limit") {
                TokenFixtures.shared.recorded().count >= 5 && observation.status.busy && observation.status.issue != nil
            }
            auth.unauthorized(); auth.restore(clientID: clientID)
            let kept = await credentials.read(account: "spotify")
            try check(kept == saved && observation.disconnected == 0, "An outage erased a valid saved login or dropped its projection")
            try await eventually("Automatic recovery did not continue at the backoff cap") { observation.status.phase == .signedIn }
            try check(observation.tokens == ["later-access"] && observation.authorizationURL == nil
                      && TokenFixtures.shared.recorded().count == failures.count + 1,
                      "Recovery opened consent or duplicated the refresh request")
            try check(observation.status.spokenProgressAccess == .unknown,
                      "A legacy Keychain record and refresh response without scope do not imply grant or denial")
            auth.shutdown()
        }
        do {
            let credentials = LockedLoginCredential(saved), observation = Observation()
            TokenFixtures.shared.reset([(200, token("unlocked-access", refresh: nil))])
            let auth = make(credentials,observation); auth.restore(clientID: clientID)
            try await eventually("Keychain interaction failure did not offer manual saved login") { observation.status.canRetrySavedLogin }
            auth.unauthorized(); try await Task.sleep(for: .milliseconds(40))
            try check(TokenFixtures.shared.recorded().isEmpty, "Locked Keychain started an automatic refresh")
            await credentials.unlock(); auth.retrySavedLogin()
            try await eventually("Explicit retry did not restore unlocked Keychain login") { observation.status.phase == .signedIn }
            try check(observation.tokens == ["unlocked-access"] && observation.authorizationURL == nil, "Keychain recovery opened browser consent")
            auth.shutdown()
        }
        for action in ["cancel","shutdown","disconnect"] {
            let credentials = MemoryCredentials(), observation = Observation()
            await credentials.write(saved,account: "spotify")
            TokenFixtures.shared.resetReplies([.failure(.networkConnectionLost),.http(200,token("must-not-arrive"))])
            let auth = make(credentials,observation,delays: [0.15]); auth.restore(clientID: clientID)
            try await eventually("Refresh never entered retry backoff") { observation.status.busy && observation.status.issue != nil }
            if action == "cancel" { auth.cancel() } else if action == "shutdown" { auth.shutdown() } else { auth.disconnect() }
            try await Task.sleep(for: .milliseconds(230))
            let remaining = await credentials.read(account: "spotify")
            try check(TokenFixtures.shared.recorded().count == 1 && observation.tokens.isEmpty && observation.authorizationURL == nil, "A cancelled backoff issued another token request")
            try check((remaining == nil) == (action == "disconnect"), "Cancel/quit/disconnect saved-login policy changed")
            auth.shutdown()
        }
        do {
            let credentials = MemoryCredentials(), observation = Observation()
            await credentials.write(saved,account: "spotify")
            TokenFixtures.shared.resetReplies([.http(429,"{}",["Retry-After":"1"]),.http(200,token("after-limit",refresh: nil))])
            let auth = make(credentials,observation); let began = ContinuousClock.now
            auth.restore(clientID: clientID)
            try await eventually("Rate-limit retry did not start") { observation.status.issue != nil }
            auth.unauthorized(); auth.restore(clientID: clientID)
            try await Task.sleep(for: .milliseconds(150))
            try check(TokenFixtures.shared.recorded().count == 1 && observation.status.busy, "A caller bypassed Retry-After during automatic recovery")
            try await eventually("Rate-limit retry did not finish") { observation.status.phase == .signedIn }
            try check(ContinuousClock.now - began >= .seconds(1) && observation.tokens == ["after-limit"], "Rate-limit recovery ran before the server wait")
            auth.shutdown()
        }
        do {
            let credentials = MemoryCredentials(), observation = Observation()
            await credentials.write(saved,account: "spotify")
            TokenFixtures.shared.resetReplies([.http(503,"{}",["Retry-After":"86401"]),.http(200,token("must-wait",refresh: nil))])
            let auth = make(credentials,observation); auth.restore(clientID: clientID)
            try await eventually("Long server cooldown did not enter automatic recovery") { observation.status.busy && observation.status.issue != nil }
            auth.unauthorized(); auth.restore(clientID: clientID); try await Task.sleep(for: .milliseconds(100))
            try check(TokenFixtures.shared.recorded().count == 1 && observation.tokens.isEmpty, "Long server cooldown was shortened")
            auth.shutdown()
        }
        for (reply, revoked) in [(TokenFixtures.Reply.http(400,"{\"error\":\"invalid_grant\"}"),true),(.http(403,"{}"),false),(.http(200,"{\"access_token\":\"fixture\",\"token_type\":\"Bearer\",\"expires_in\":3600,\"scope\":\"user-library-read\"}"),false)] {
            let credentials = MemoryCredentials(), observation = Observation()
            await credentials.write(saved,account: "spotify"); TokenFixtures.shared.resetReplies([reply])
            let auth = make(credentials,observation); auth.restore(clientID: clientID)
            try await eventually("Permanent authorization error did not finish") { observation.status.issue != nil && !observation.status.busy }
            auth.unauthorized()
            try await Task.sleep(for: .milliseconds(60))
            let remaining = await credentials.read(account: "spotify")
            try check(TokenFixtures.shared.recorded().count == 1 && !observation.status.canRetrySavedLogin && observation.tokens.isEmpty && (remaining == nil) == revoked, "Permanent errors were retried or saved-login revocation changed")
            auth.shutdown()
        }
        print("Refresh recovery passed: automatic capped backoff, Retry-After, cancellation, revocation and unchanged grants.")
    }
    @MainActor static func testRestoredLoginAndExpiry() async throws {
        let credentials = MemoryCredentials(), observation = Observation()
        let saved = try JSONSerialization.data(withJSONObject: ["clientID": String(repeating: "b", count: 32), "refreshToken": "restored-refresh"])
        await credentials.write(saved, account: "spotify")
        TokenFixtures.shared.reset([(200, token("restored-access", refresh: nil, expiry: 61)), (200, token("timer-access", refresh: "timer-rotation"))])
        var browserOpens = 0
        let auth = SpotifyAuthorization(credentials: credentials, client: oauthClient(), status: { observation.status = $0 }, access: { observation.tokens.append($0) }, disconnected: { observation.disconnected += 1 }, openBrowser: { _ in browserOpens += 1; return false }, callbackPort: nil)
        auth.restore(clientID: String(repeating: "b", count: 32))
        try check(observation.status.phase == .refreshing, "Restoration immediately shows progress")
        try await eventually("Saved login and expiry timer refreshed without browser", { observation.tokens.count == 2 })
        try check(observation.tokens == ["restored-access", "timer-access"] && browserOpens == 0 && observation.disconnected == 0, "Restore and timed refresh lifecycle")
        let requests = TokenFixtures.shared.recorded()
        try check(requests.count == 2 && requests.allSatisfy { parameters(String(decoding: $0.httpBody ?? Data(), as: UTF8.self))["grant_type"] == "refresh_token" }, "Restoration uses refresh grants")
        auth.shutdown()
        let remaining = await credentials.read(account: "spotify")
        try check(remaining != nil, "Normal shutdown keeps saved login")
    }

}
