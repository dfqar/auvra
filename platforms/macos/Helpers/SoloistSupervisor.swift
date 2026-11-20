import Foundation
import Darwin
import OSLog

/// A pipe-bound child supervisor: EOF (including Auvra crash) stops only our child.
/// The startup key arrives through stdin, never in this helper's arguments or output.
private struct LaunchRequest: Decodable {
    let executable, dataDirectory, cacheDirectory, deviceName, apiKey: String
    let initialVolume: UInt8?
}
private struct ControlRequest: Decodable, Sendable {
    struct Control: Decodable, Sendable {
        let kind: String
        let positionMs: UInt64?
        let percent: UInt8?
        let enabled: Bool?
        let mode, uri: String?
    }
    let id, sequence, expiresAtUptimeMs: UInt64
    let requiredQueueEpoch: UInt64?
    let mediaId: String
    let control: Control
}
/// Bootstrap has no media, output clock or provider capability. The private
/// launch pipe scopes it to the one child owned by this supervisor.
private struct ActivationRequest: Decodable, Sendable {
    let type, action: String
    let id, expiresAtUptimeMs: UInt64
    let expiresAtContinuousNs: UInt64
}
private struct BootstrapHeader: Decodable {
    let type: String
    let id: UInt64
}
private enum ActivationClockPolicy {
    static func timelyReceipt(continuous: UInt64, deadline: UInt64) -> Bool {
        deadline > 0 && continuous <= deadline
    }
    static func freshState(now: Double, lastState: Double, continuous: UInt64, lastContinuous: UInt64) -> Bool {
        now.isFinite && lastState.isFinite && now >= lastState && now - lastState <= 0.9
            && lastContinuous > 0 && continuous >= lastContinuous && continuous - lastContinuous <= 900_000_000
    }
}
/// The blocking pipe reader records cancellation before scheduling actor work.
/// A queued activation must not race that record on a different executor.
private final class ActivationCancellationFence: @unchecked Sendable {
    private let lock = NSLock()
    private var through: UInt64 = 0
    func cancel(_ id: UInt64) { lock.lock(); defer { lock.unlock() }; through = max(through, id) }
    func permits(_ id: UInt64) -> Bool { lock.lock(); defer { lock.unlock() }; return id > through }
}
private enum PipeRequest: Sendable {
    case control(ControlRequest)
    case activate(ActivationRequest)
}
private let outputLock = NSLock()
private func emit(_ value: [String: Any]) {
    guard var data = try? JSONSerialization.data(withJSONObject: value), data.count <= 48 * 1024 else { return }
    data.append(10)
    outputLock.lock(); defer { outputLock.unlock() }
    try? FileHandle.standardOutput.write(contentsOf: data)
}
/// Only authentication and compact observations leave this adapter. Raw entities,
/// credentials and runtime logs stay here. The provider chooses the command route.
private actor SoloistLocalControl {
    private static let trace = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
    private static let logger = Logger(subsystem: "app.auvra.player", category: "SoloistSupervisor")
    private static let commandKinds: Set<String> = ["play", "pause", "next", "previous", "previous_from_start", "start_uri", "seek", "volume", "shuffle", "repeat", "enqueue", "autoplay_enqueue", "activate"]
    private static let previousCueMaxPosition = 180.0
    private enum CommandPhase: String { case sending, accepted, rejected, timeout }
    private enum PlaybackPhase: String { case clockCleared = "clock_cleared", incompleteHeld = "incomplete_held", previousSnapshot = "previous_snapshot" }
    private enum PlaybackReason: String { case activeIncomplete = "active_incomplete", expired, auth, socket, invalid, inactive }
    private enum NextQueueRejection: String {
        case requestEpoch = "request_epoch", unavailable, missingRows = "missing_rows", stateStale = "state_stale"
    }
    private enum NextScopeReset: String {
        case socket, auth, expired, superseded, incomplete, fullState = "full_state"
        case trackChanged = "track_changed", playbackIdle = "playback_idle", deviceChanged = "device_changed"
        case contextChanged = "context_changed", stateCleared = "state_cleared", confirmed
    }
    private enum JSONShape: String { case missing, null, object, array, string, number, other }
    private enum URIKind: String { case track, episode, chapter, other, unavailable }
    private enum URILength: String { case zero, expected, other, unavailable }

    private func traceCommand(_ phase: CommandPhase, _ control: ControlRequest.Control) {
        guard Self.trace else { return }
        let kind = Self.commandKinds.contains(control.kind) ? control.kind : "unknown"
        Self.logger.info("soloist_command phase=\(phase.rawValue, privacy: .public) kind=\(kind, privacy: .public)")
    }
    private func tracePlayback(_ phase: PlaybackPhase, _ reason: PlaybackReason, status: String = "unknown",
                               hasMedia: Bool = false, hasPosition: Bool = false, active: Bool = false) {
        guard Self.trace else { return }
        let allowedStatus = ["idle", "playing", "paused", "buffering"].contains(status) ? status : "unknown"
        Self.logger.info("soloist_clock phase=\(phase.rawValue, privacy: .public) reason=\(reason.rawValue, privacy: .public) status=\(allowedStatus, privacy: .public) media=\(hasMedia, privacy: .public) position=\(hasPosition, privacy: .public) active=\(active, privacy: .public) relative=\(self.relativeTransition != nil, privacy: .public)")
    }
    private func traceNextArming(media: String?, hasAnchor: Bool, armed: Bool, now: Double) {
        guard Self.trace else { return }
        let repeatMode = options?["repeat"] as? String
        let rows = queuePrefix?.rows.count ?? 0
        let queueAgeMs = queuePrefix.map { (now - $0.observedAt) * 1_000 } ?? -1
        let sameHead = media != nil && queuePrefix?.rows.first?.media == media
        let independentHead = queuePrefix?.uniqueHead != nil
        Self.logger.info("soloist_same_next stage=arm armed=\(armed, privacy: .public) has_media=\(media != nil, privacy: .public) anchor=\(hasAnchor, privacy: .public) rows=\(rows, privacy: .public) queue_age_ms=\(queueAgeMs, privacy: .public) same_head=\(sameHead, privacy: .public) independent_head=\(independentHead, privacy: .public) repeat_known=\(repeatMode != nil, privacy: .public) repeat_one=\(repeatMode == "one", privacy: .public)")
    }
    private func traceNextReset(_ reason: NextScopeReset, mediaChanged: Bool = false,
                                contextChanged: Bool = false, activeChanged: Bool = false) {
        guard Self.trace, let transition = relativeTransition, transition.duplicateNext != nil else { return }
        let elapsedMs = (ProcessInfo.processInfo.systemUptime - transition.sentAt) * 1_000
        Self.logger.info("soloist_same_next stage=reset reason=\(reason.rawValue, privacy: .public) elapsed_ms=\(elapsedMs, privacy: .public) media_changed=\(mediaChanged, privacy: .public) context_changed=\(contextChanged, privacy: .public) active_changed=\(activeChanged, privacy: .public)")
    }
    private func traceNextQueueRejection(_ reason: NextQueueRejection) {
        guard Self.trace, relativeTransition?.duplicateNext != nil else { return }
        Self.logger.info("soloist_same_next stage=queue_rejected reason=\(reason.rawValue, privacy: .public)")
    }
    private func traceNextQueue(_ observed: QueuePrefix?, count: Int, parsed: Int, distinct: Int,
                                requested: Bool, advanced: Bool, now: Double) {
        guard Self.trace else { return }
        Self.logger.info("soloist_same_next stage=queue_parse count=\(count, privacy: .public) parsed=\(parsed, privacy: .public) distinct=\(distinct, privacy: .public) request_outstanding=\(requested, privacy: .public) valid=\(observed != nil, privacy: .public)")
        guard let transition = relativeTransition, let witness = transition.duplicateNext else { return }
        let elapsedMs = (now - transition.sentAt) * 1_000
        let oldCount = witness.queue.count
        let tailCount = witness.queue.rows.count - 1
        let countMatches = count == oldCount - 1 || count == QueuePrefix.limit && oldCount == QueuePrefix.limit
        let headGone = observed.map { next in !next.rows.contains { $0.uid == witness.queue.rows.first?.uid } } ?? false
        let tailMatches = observed.map { witness.queue.rows.dropFirst().elementsEqual($0.rows.prefix(tailCount)) } ?? false
        Self.logger.info("soloist_same_next stage=queue elapsed_ms=\(elapsedMs, privacy: .public) previous_count=\(oldCount, privacy: .public) scope=\(self.duplicateNextScope(transition, witness), privacy: .public) context_same=\(witness.context == self.queueContext, privacy: .public) epoch_same=\(witness.epoch == self.queueEpoch, privacy: .public) count_matches=\(countMatches, privacy: .public) head_gone=\(headGone, privacy: .public) tail_matches=\(tailMatches, privacy: .public) advanced=\(advanced, privacy: .public)")
    }
    private func traceNextSnapshot(state: String, full: Bool, sameMedia: Bool, isActive: Bool, position: Double?, rate: Double?) {
        guard Self.trace, let transition = relativeTransition, let witness = transition.duplicateNext else { return }
        let repeatMode = options?["repeat"] as? String
        let elapsedMs = (ProcessInfo.processInfo.systemUptime - transition.sentAt) * 1_000
        Self.logger.info("soloist_same_next stage=snapshot elapsed_ms=\(elapsedMs, privacy: .public) state=\(state, privacy: .public) full=\(full, privacy: .public) scope=\(self.duplicateNextScope(transition, witness), privacy: .public) same_media=\(sameMedia, privacy: .public) active=\(isActive, privacy: .public) position_ms=\(position ?? -1, privacy: .public) zero_rate=\(rate == 0, privacy: .public) positive_rate=\((rate ?? 0) > 0, privacy: .public) actions=\(self.actions != nil, privacy: .public) repeat_known=\(repeatMode != nil, privacy: .public) repeat_one=\(repeatMode == "one", privacy: .public) buffered=\(witness.observedBuffering, privacy: .public) playing=\(witness.confirmedPlaying, privacy: .public)")
    }
    private func traceItemShape(_ item: Any?, _ uri: Any?) {
        guard Self.trace else { return }
        func shape(_ value: Any?) -> JSONShape {
            switch value {
            case nil: return .missing
            case is NSNull: return .null
            case is [String: Any]: return .object
            case is [Any]: return .array
            case is String: return .string
            case is NSNumber: return .number
            default: return .other
            }
        }
        let itemShape = shape(item)
        let uriShape = shape(uri)
        var kind: URIKind = .unavailable
        var length: URILength = .unavailable
        if let uri = uri as? String {
            let parts = uri.split(separator: ":", maxSplits: 3, omittingEmptySubsequences: false)
            length = uri.isEmpty ? .zero : parts.count == 3 && parts[2].utf8.count == 22 ? .expected : .other
            if parts.count == 3, parts[0] == "spotify" {
                switch parts[1] {
                case "track": kind = .track
                case "episode": kind = .episode
                case "chapter": kind = .chapter
                default: kind = .other
                }
            } else {
                kind = .other
            }
        }
        Self.logger.info("soloist_item_shape item=\(itemShape.rawValue, privacy: .public) uri=\(uriShape.rawValue, privacy: .public) kind=\(kind.rawValue, privacy: .public) length=\(length.rawValue, privacy: .public)")
    }
    private struct Step {
        let name: String
        var fields: [String: Any] = [:]
    }
    private struct InFlight {
        let request: ControlRequest
        let ackDeadline: Double
        var remaining: [Step]
        var previousCue: PreviousCue? = nil
        var playback: PlaybackWitness? = nil
    }
    private struct PlaybackWitness {
        let media, state: String
        let context: String?
        let epoch, sequence: UInt64
        var observedAt: Double?

        func matches(media: String?, context: String?, state: String, rate: Double) -> Bool {
            self.media == media && self.context == context && self.state == state
                && (state == "playing" ? rate > 0 : rate == 0)
        }
    }
    private struct SocketRecovery {
        let playback: PlaybackWitness
        let port: UInt16
        let deadline: Double
        var authenticated = false
    }
    private struct PreviousCue {
        let context, state: String
        var seek: (sequence: UInt64, wallMs: Double)?
        var confirmedAt: Double?
        var previousSent = false
    }
    private struct PreviousQueueRefresh {
        let id, sequence, epoch: UInt64
        let media, context: String
        let deadline: Double
        var acknowledged = false
        var successor: (media: String, epoch: UInt64)?
    }
    private struct QueueOccurrence: Equatable {
        let uid, media, source: String
    }
    private struct QueuePrefix {
        static let limit = 10
        let rows: [QueueOccurrence]
        let count: Int
        let observedAt: Double

        init?(rows: [QueueOccurrence], count: Int, observedAt: Double) {
            guard rows.count == min(Self.limit, count) else { return nil }
            var identities: [String: QueueOccurrence] = [:]
            for row in rows {
                if let earlier = identities.updateValue(row, forKey: row.uid),
                   row.source != "context" || earlier != row { return nil }
            }
            self.rows = rows; self.count = count; self.observedAt = observedAt
        }

        var uniqueHead: QueueOccurrence? {
            // Repeat Context can reuse the same IDs in subsequent cycles.
            // The removed occurrence must still be unique in the sampled window.
            guard rows.count >= 3, let head = rows.first, rows[1].uid != rows[2].uid,
                  !rows.dropFirst().contains(where: { $0.uid == head.uid }) else { return nil }
            return head
        }

        func removesHead(in next: Self) -> Bool {
            let countMatches = next.count == count - 1 || next.count == Self.limit && count == Self.limit
            guard let head = uniqueHead, countMatches,
                  next.rows.count >= rows.count - 1, !next.rows.contains(where: { $0.uid == head.uid }) else { return false }
            return rows.dropFirst().elementsEqual(next.rows.prefix(rows.count - 1))
        }
    }
    private struct DuplicateNext {
        let wireID: UInt64
        let queue: QueuePrefix
        let epoch: UInt64
        let context: String?
        var acknowledged = false
        var queueAdvanced = false
        var observedBuffering = false
        var confirmedPlaying = false
    }
    private struct RelativeTransition {
        let sentAt: Double
        let media: String
        let position: Double
        let canRestart: Bool
        var duplicateNext: DuplicateNext? = nil
        var incomplete = false
        var observedEmptyURI = false
        var standardDeadline: Double { sentAt + 1.5 }
        var deadline: Double { observedEmptyURI ? sentAt + 3.5 : standardDeadline }
    }
    private var socket: URLSessionWebSocketTask?
    private var socketPort: UInt16?
    private var socketRecovery: SocketRecovery?
    private var loggedIn = false, active = false
    private var media: String?
    private var status = "idle"
    private struct PositionAnchor {
        let position, uptime, rate: Double
        var sourceTimestampMs: Double? = nil
    }
    private var anchor: PositionAnchor?
    private var previousTrack: (uri: String, until: Double)?
    private var sequence: UInt64 = 0
    private var lastStateAt = ProcessInfo.processInfo.systemUptime
    private var lastStateContinuousNs: UInt64 = 0
    private var receivedAuth = false
    private var failures = 0
    private var volume: Double?
    private var options: [String: Any]?
    private var actions: [String: Any]?
    private var metadata: [String: Any]?
    private var lastQueryAt: Double = 0
    private var minimumControlSequence: UInt64 = 1
    private var minimumSessionControlSequence: UInt64 = 1
    private var scopedPlayPausedSince: UInt64?
    private var lastControlID: UInt64 = 0
    private var inFlight: InFlight?
    private var activation: (request: ActivationRequest, connection: URLSessionWebSocketTask, receiptDeadline: UInt64)?
    private var activationTimeout: Task<Void, Never>?
    nonisolated private let activationCancellation = ActivationCancellationFence()
    private var controlTimeout: Task<Void, Never>?
    private var playAckGrace: Task<Void, Never>?
    private var playAckFence: Double?
    private var repeatTransition: (previous: String, requested: String, expires: Double)?
    private var relativeTransition: RelativeTransition?
    private var queueEpoch: UInt64 = 1
    private var queuePrefix: QueuePrefix?
    private var queueSequence: UInt64 = 0
    private var queueContext: String?
    private var queueReady = false
    private var nextQueueAt: Double = 0
    private var queueRequest: (epoch: UInt64, expires: Double)?
    private var previousQueueRefresh: PreviousQueueRefresh?
    private var emptyTail: (epoch: UInt64, since: Double, observedAt: Double)?
    private var nextTailQueueAt: Double = 0
    private var autoplayAttemptedEpoch: UInt64?
    private var minimumAutoplaySequence: UInt64 = 0
    func connect(directory: String, pid: Int32, launched: Date) async {
        let base = URL(fileURLWithPath: directory)
        var discoveryAttempts = 0
        while !Task.isCancelled {
            if Task.isCancelled { return }
            if let pidText = bounded(base.appendingPathComponent("soloist.pid")), Int32(pidText) == pid,
               let address = bounded(base.appendingPathComponent("ws.addr")), address == "127.0.0.1",
               let portText = bounded(base.appendingPathComponent("ws.port")), let port = UInt16(portText), port > 0,
               let values = try? base.appendingPathComponent("ws.port").resourceValues(forKeys: [.contentModificationDateKey]),
               let modified = values.contentModificationDate, modified.timeIntervalSince(launched) >= -0.05 {
                if let recovery = socketRecovery,
                   recovery.port != port || ProcessInfo.processInfo.systemUptime >= recovery.deadline {
                    clearSocketState(notify: true)
                }
                var recovering = socketRecovery != nil
                let config = URLSessionConfiguration.ephemeral
                config.urlCache = nil; config.httpCookieStorage = nil
                let session = URLSession(configuration: config)
                let connection = session.webSocketTask(with: URL(string: "ws://127.0.0.1:\(port)")!)
                connection.maximumMessageSize = 256 * 1024; socket = connection; socketPort = port; connection.resume()
                discoveryAttempts = 0; receivedAuth = false
                lastStateAt = ProcessInfo.processInfo.systemUptime
                lastStateContinuousNs = 0
                let recoveryTimeout = socketRecovery.map { recovery in
                    Task { [weak self] in
                        try? await Task.sleep(for: .seconds(max(0, recovery.deadline - ProcessInfo.processInfo.systemUptime)))
                        guard !Task.isCancelled else { return }
                        await self?.expireSocketRecovery(connection)
                    }
                }
                let heartbeat = Task { [weak self] in
                    while !Task.isCancelled {
                        try? await Task.sleep(for: .milliseconds(250))
                        guard !Task.isCancelled, let self else { return }
                        await self.refresh(connection)
                    }
                }
                var disconnected = false
                do {
                    while !Task.isCancelled {
                        let received = try await connection.receive()
                        guard socket === connection else { break }
                        let data: Data
                        switch received { case .data(let value): data = value; case .string(let value): data = Data(value.utf8); @unknown default: continue }
                        guard data.count <= 256 * 1024, let message = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { continue }
                        if socketRecovery != nil {
                            guard resumeSocketObservation(message, connection: connection) else { continue }
                            recovering = false
                            recoveryTimeout?.cancel()
                        }
                        if await receiveControlResult(message, connection: connection) { continue }
                        let needsState = receive(message)
                        if inFlight?.previousCue != nil { await advancePreviousCue(connection) }
                        if needsState { await refresh(connection) }
                        else if previousQueueRefresh?.successor != nil {
                            await refreshQueue(connection, now: ProcessInfo.processInfo.systemUptime, onlyAfterPrevious: true)
                        }
                    }
                } catch { disconnected = true }
                heartbeat.cancel(); recoveryTimeout?.cancel(); socket = nil; socketPort = nil
                playAckFence = nil
                connection.cancel(); session.invalidateAndCancel()
                if !recovering && socketRecovery != nil { continue }
                clearSocketState(notify: disconnected || socketRecovery != nil)
                failures = min(4, failures + 1)
                try? await Task.sleep(for: .seconds(1 << failures))
                continue
            }
            if socketRecovery != nil { clearSocketState(notify: true) }
            discoveryAttempts += 1
            if discoveryAttempts == 50 { emit(["event":"local_control_unavailable"]) }
            try? await Task.sleep(for: .milliseconds(100))
        }
    }
    private func clearSocketState(notify: Bool) {
        socketRecovery = nil
        playAckFence = nil
        if notify { emit(["event":"local_control_unavailable"]) }
        tracePlayback(.clockCleared, .socket, status: status, hasMedia: media != nil, hasPosition: anchor != nil, active: active)
        finishControl(accepted: false)
        finishActivation(accepted: false)
        loggedIn = false; active = false; anchor = nil; media = nil; previousTrack = nil
        traceNextReset(.socket)
        volume = nil; options = nil; metadata = nil; repeatTransition = nil; relativeTransition = nil
        queueRequest = nil; queueContext = nil; invalidateQueue(.socket)
        emit(["event":"clock","clock":NSNull()])
    }
    private func expireSocketRecovery(_ connection: URLSessionWebSocketTask) {
        guard socket === connection, socketRecovery != nil else { return }
        socket = nil
        connection.cancel(with: .goingAway, reason: nil)
    }
    private func resumeSocketObservation(_ message: [String: Any], connection: URLSessionWebSocketTask) -> Bool {
        guard let recovery = socketRecovery else { return true }
        guard ProcessInfo.processInfo.systemUptime < recovery.deadline else {
            expireSocketRecovery(connection); return false
        }
        switch message["type"] as? String {
        case "auth_state":
            guard boolean(message["logged_in"]) == true, boolean(message["is_active"]) == true else {
                expireSocketRecovery(connection); return false
            }
            socketRecovery?.authenticated = true
            receivedAuth = true
        case "playback_state":
            guard recovery.authenticated, boolean(message["is_active"]) == true,
                  let state = message["status"] as? String, let position = position(message["position"]), let anchor,
                  recovery.playback.matches(media: identifier((message["item"] as? [String: Any])?["uri"]),
                                            context: contextURI(message), state: state, rate: position.rate),
                  position.rate == anchor.rate else {
                expireSocketRecovery(connection); return false
            }
            // The same URI may recur in the queue while the wire is unavailable.
            // Preserve its epoch only if the new anchor follows the old clock.
            let projected = anchor.position + max(0, position.uptime - anchor.uptime) * anchor.rate * 1000
            guard abs(position.position - projected) <= 250 else {
                expireSocketRecovery(connection); return false
            }
            socketRecovery = nil
            if Self.trace {
                Self.logger.info("soloist_ack_recovery stage=confirmed state=\(state, privacy: .public)")
            }
            return true
        case "device_changed":
            if boolean(message["is_active"]) == false { expireSocketRecovery(connection) }
        default: break
        }
        return false
    }
    private func readySocket(for request: ControlRequest) async -> URLSessionWebSocketTask? {
        while let recovery = socketRecovery, !Task.isCancelled, request.id == lastControlID,
              ProcessInfo.processInfo.systemUptime < recovery.deadline {
            try? await Task.sleep(for: .milliseconds(4))
        }
        guard socketRecovery == nil, request.id == lastControlID, inFlight == nil, activation == nil, !Task.isCancelled else { return nil }
        return socket
    }
    private func activationEligible(_ request: ActivationRequest, connection: URLSessionWebSocketTask) -> Bool {
        let now = ProcessInfo.processInfo.systemUptime
        let ms = UInt64(now * 1000)
        let continuous = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
        return !Task.isCancelled && request.type == "bootstrap" && request.action == "activate"
            && activationCancellation.permits(request.id) && request.id == lastControlID
            && request.expiresAtUptimeMs >= ms && request.expiresAtUptimeMs - ms <= 1500
            && request.expiresAtContinuousNs >= continuous && request.expiresAtContinuousNs - continuous <= 1_500_000_000
            && socket === connection && socketRecovery == nil && receivedAuth && loggedIn
            && ActivationClockPolicy.freshState(now: now, lastState: lastStateAt, continuous: continuous, lastContinuous: lastStateContinuousNs)
    }
    func dispatchActivation(_ request: ActivationRequest) async {
        guard request.id > lastControlID else {
            emit(["event":"activation_result", "id":request.id, "accepted":false]); return
        }
        lastControlID = request.id
        guard let connection = socket, inFlight == nil, activation == nil,
              activationEligible(request, connection: connection) else {
            emit(["event":"activation_result", "id":request.id, "accepted":false]); return
        }
        let (receiptDeadline, overflow) = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW).addingReportingOverflow(1_200_000_000)
        guard !overflow else {
            emit(["event":"activation_result", "id":request.id, "accepted":false]); return
        }
        activation = (request, connection, receiptDeadline)
        activationTimeout = Task { [weak self] in
            do { try await Task.sleep(for: .milliseconds(1200)) } catch { return }
            await self?.retireActivation(id: request.id, connection: connection)
        }
        // Recheck the original host deadline immediately before the only write.
        // No output/media/action availability is inferred by this bootstrap.
        guard activationEligible(request, connection: connection) else {
            finishActivation(accepted: false); return
        }
        if Self.trace { Self.logger.info("soloist_activation phase=sending wire=\(request.id, privacy: .public)") }
        do {
            try await connection.send(.string("{\"type\":\"command\",\"command\":\"activate\"}"))
        } catch { retireActivation(id: request.id, connection: connection) }
    }
    func cancelActivation(_ id: UInt64) {
        recordActivationCancellation(id)
        guard let pending = activation, pending.request.id == id else {
            emit(["event":"activation_result", "id":id, "accepted":false]); return
        }
        retireActivation(id: id, connection: pending.connection)
    }
    nonisolated func recordActivationCancellation(_ id: UInt64) { activationCancellation.cancel(id) }
    private func retireActivation(id: UInt64, connection: URLSessionWebSocketTask) {
        guard activation?.request.id == id, activation?.connection === connection else { return }
        // ACKs carry a command name only. A canceled/timed-out activation must
        // never acknowledge a later invocation on the same WebSocket.
        if socket === connection {
            socket = nil; socketRecovery = nil
            connection.cancel(with: .goingAway, reason: nil)
        }
        finishActivation(accepted: false)
    }
    private func finishActivation(accepted: Bool) {
        guard let pending = activation else { return }
        activation = nil; activationTimeout?.cancel(); activationTimeout = nil
        if Self.trace { Self.logger.info("soloist_activation phase=finished wire=\(pending.request.id, privacy: .public) accepted=\(accepted, privacy: .public)") }
        emit(["event":"activation_result", "id":pending.request.id, "accepted":accepted])
    }
    private func socketForPlay(_ connection: URLSessionWebSocketTask, request: ControlRequest) async -> URLSessionWebSocketTask? {
        while let deadline = playAckFence, ProcessInfo.processInfo.systemUptime < deadline,
              socket === connection, request.id == lastControlID, !Task.isCancelled, sessionEligible(request) {
            try? await Task.sleep(for: .milliseconds(4))
        }
        guard socket === connection, request.id == lastControlID, inFlight == nil,
              !Task.isCancelled, eligible(request) else { return nil }
        if playAckFence == nil { return connection }
        // A new plain Play or URI start would also be acknowledged as "play".
        // Replace the old socket before writing either, and only on a fresh full clock.
        guard let media, actions != nil, status == "playing" || status == "paused",
              let recovery = playbackRecovery(PlaybackWitness(media: media, state: status, context: queueContext,
                                                              epoch: queueEpoch, sequence: sequence, observedAt: lastStateAt)) else { return nil }
        socketRecovery = recovery
        socket = nil
        if Self.trace { Self.logger.info("soloist_ack_recovery stage=replacing_wire cause=undrained_play") }
        connection.cancel(with: .goingAway, reason: nil)
        return await readySocket(for: request)
    }
    func dispatch(_ request: ControlRequest) async {
        guard request.id > lastControlID else { return }
        lastControlID = request.id
        guard var connection = await readySocket(for: request) else {
            traceCommand(.rejected, request.control)
            emit(["event":"control_result", "id":request.id, "accepted":false]); return
        }
        if isOutputTransport(request.control) {
            // A granular transition clears action availability until the next full state.
            // Keep the original dispatch expiry and send at most once.
            while socket === connection, !Task.isCancelled, sessionEligible(request), matchesRequiredOccurrence(request),
                  actions == nil || ProcessInfo.processInfo.systemUptime - lastStateAt > 0.9
                    || (isPlaybackControl(request.control) && status == "buffering" && steps(for: request.control) == nil) {
                try? await Task.sleep(for: .milliseconds(20))
            }
        } else if request.control.kind == "repeat" || request.control.kind == "shuffle" {
            // An options event can clear the action snapshot after admission.
            // Wait only on the original item and within the original expiry.
            while socket === connection, !Task.isCancelled, sessionEligible(request),
                  itemEligible(request), actions == nil {
                try? await Task.sleep(for: .milliseconds(20))
            }
        }
        guard socket === connection, inFlight == nil, activation == nil, !Task.isCancelled, eligible(request) else {
            traceCommand(.rejected, request.control)
            emit(["event":"control_result", "id":request.id, "accepted":false]); return
        }
        if alreadyInRequestedState(request.control) {
            traceCommand(.accepted, request.control)
            emit(["event":"control_result", "id":request.id, "accepted":true]); return
        }
        guard var requestedSteps = steps(for: request.control) else {
            traceCommand(.rejected, request.control)
            emit(["event":"control_result", "id":request.id, "accepted":false]); return
        }
        if requestedSteps.first?.name == "play", playAckFence != nil {
            guard let usable = await socketForPlay(connection, request: request), eligible(request) else {
                traceCommand(.rejected, request.control)
                emit(["event":"control_result", "id":request.id, "accepted":false]); return
            }
            connection = usable
            if alreadyInRequestedState(request.control) {
                traceCommand(.accepted, request.control)
                emit(["event":"control_result", "id":request.id, "accepted":true]); return
            }
            guard let currentSteps = steps(for: request.control) else {
                traceCommand(.rejected, request.control)
                emit(["event":"control_result", "id":request.id, "accepted":false]); return
            }
            requestedSteps = currentSteps
        }
        let previousCue: PreviousCue?
        if request.control.kind == "previous_from_start" {
            guard let context = queueContext, status == "paused" || status == "playing",
                  let anchor, status == "paused" ? anchor.rate == 0 : anchor.rate > 0 else {
                traceCommand(.rejected, request.control)
                emit(["event":"control_result", "id":request.id, "accepted":false]); return
            }
            previousCue = PreviousCue(context: context, state: status)
        } else {
            previousCue = nil
        }
        if ["play", "pause", "start_uri", "seek", "next", "previous", "previous_from_start", "repeat", "shuffle", "enqueue"].contains(request.control.kind) {
            emptyTail = nil
            minimumAutoplaySequence = sequence &+ 1
        }
        let ackDeadline = ProcessInfo.processInfo.systemUptime + 1.2
        inFlight = InFlight(request: request, ackDeadline: ackDeadline, remaining: requestedSteps, previousCue: previousCue)
        if request.control.kind == "repeat", let requested = request.control.mode,
           let previous = options?["repeat"] as? String, previous != requested {
            repeatTransition = (previous, requested, ProcessInfo.processInfo.systemUptime + 1.5)
        }
        controlTimeout = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0, ackDeadline - ProcessInfo.processInfo.systemUptime)))
            guard !Task.isCancelled else { return }
            await self?.failCommand(id: request.id, connection: connection, timeout: true)
        }
        await sendStep(connection)
    }
    private func isOutputTransport(_ control: ControlRequest.Control) -> Bool {
        control.kind == "next" || control.kind == "previous" || control.kind == "previous_from_start" || isPlaybackControl(control)
    }
    private func isPlaybackControl(_ control: ControlRequest.Control) -> Bool {
        control.kind == "play" || control.kind == "pause"
    }
    private func alreadyInRequestedState(_ control: ControlRequest.Control) -> Bool {
        (control.kind == "pause" && status == "paused")
            || (control.kind == "play" && status == "playing" && (anchor?.rate ?? 0) > 0)
    }
    private func sessionEligible(_ request: ControlRequest) -> Bool {
        let ms = UInt64(ProcessInfo.processInfo.systemUptime * 1000)
        return loggedIn && active && request.sequence >= minimumSessionControlSequence && request.sequence <= sequence
            && request.expiresAtUptimeMs >= ms && request.expiresAtUptimeMs - ms <= 1500
    }
    private func itemEligible(_ request: ControlRequest) -> Bool {
        request.mediaId == media && request.sequence >= minimumControlSequence
    }
    private var isPausedForScopedPlay: Bool {
        guard loggedIn, active, status == "paused", let anchor else { return false }
        return anchor.rate == 0 && anchor.position <= 120
    }
    private func matchesRequiredOccurrence(_ request: ControlRequest) -> Bool {
        guard let requiredEpoch = request.requiredQueueEpoch else { return request.control.kind != "previous_from_start" }
        guard requiredEpoch > 0, requiredEpoch == queueEpoch, itemEligible(request) else { return false }
        switch request.control.kind {
        case "pause", "previous_from_start", "next":
            return true
        case "play":
            return isPausedForScopedPlay && (actions?["play"] as? Bool) == true
                && scopedPlayPausedSince.map { request.sequence >= $0 } == true
        default:
            return false
        }
    }
    private func eligible(_ request: ControlRequest) -> Bool {
        let now = ProcessInfo.processInfo.systemUptime
        guard sessionEligible(request), now - lastStateAt <= 0.9 else { return false }
        // Bound controls cannot use the previous-item handoff.
        if request.requiredQueueEpoch != nil || request.control.kind == "previous_from_start" {
            return matchesRequiredOccurrence(request)
        }
        if request.control.kind == "next" || request.control.kind == "previous" {
            return media != nil && identifier(request.mediaId) != nil
        }
        if isPlaybackControl(request.control), let previousTrack,
           request.mediaId == previousTrack.uri, now < previousTrack.until,
           request.sequence < minimumControlSequence, media != nil {
            return true
        }
        guard itemEligible(request) else { return false }
        if request.control.kind == "autoplay_enqueue" {
            guard request.sequence >= minimumAutoplaySequence, autoplayTailReady(at: now),
                  let remaining = autoplayRemaining(at: now), (1...1600).contains(remaining) else { return false }
        }
        return true
    }
    private func steps(for control: ControlRequest.Control) -> [Step]? {
        func action(_ key: String, _ name: String, fields: [String: Any] = [:]) -> [Step]? {
            (actions?[key] as? Bool) == true ? [Step(name: name, fields: fields)] : nil
        }
        switch control.kind {
        case "play": return action("play", "play")
        case "start_uri":
            guard let uri = identifier(control.uri, kinds: ["track", "episode", "album", "playlist"]) else { return nil }
            return [Step(name: "play", fields: ["uri":uri])]
        case "pause": return action("pause", "pause")
        case "next": return action("next", "skip_next")
        case "previous": return action("previous", "skip_prev")
        case "previous_from_start":
            guard let cue = action("seek", "seek", fields: ["position_ms":0]),
                  let previous = action("previous", "skip_prev") else { return nil }
            return cue + previous
        case "seek":
            guard let position = control.positionMs, position <= 86_400_000 else { return nil }
            return action("seek", "seek", fields: ["position_ms":position])
        case "volume":
            guard let percent = control.percent, percent <= 100, volume != nil else { return nil }
            return [Step(name: "set_volume", fields: ["volume":percent])]
        case "shuffle":
            guard let enabled = control.enabled else { return nil }
            return action("shuffle", "set_shuffle", fields: ["enabled":enabled])
        case "repeat":
            guard (actions?["repeat"] as? Bool) == true, let mode = control.mode, ["off","all","one"].contains(mode) else { return nil }
            let context = Step(name: "set_repeat_context", fields: ["enabled":mode == "all"])
            let track = Step(name: "set_repeat_track", fields: ["enabled":mode == "one"])
            return mode == "one" ? [context, track] : [track, context]
        case "enqueue", "autoplay_enqueue":
            guard let uri = identifier(control.uri, kinds: ["track"]) else { return nil }
            if control.kind == "autoplay_enqueue", uri == media { return nil }
            return action("queue_add", "add_to_queue", fields: ["uri":uri])
        default: return nil
        }
    }
    private func sendStep(_ connection: URLSessionWebSocketTask) async {
        guard socket === connection, let command = inFlight, let step = command.remaining.first else { return }
        guard eligible(command.request) else { finishControl(accepted: false); return }
        if isPlaybackControl(command.request.control), itemEligible(command.request), let media {
            inFlight?.playback = PlaybackWitness(media: media, state: step.name == "play" ? "playing" : "paused",
                                                context: queueContext, epoch: queueEpoch, sequence: command.request.sequence)
        }
        if let cue = command.previousCue {
            guard cue.context == queueContext, cue.state == status else {
                failCommand(id: command.request.id, connection: connection); return
            }
            if step.name == "seek" {
                guard cue.seek == nil, steps(for: command.request.control) != nil else {
                    failCommand(id: command.request.id, connection: connection); return
                }
                inFlight?.previousCue?.seek = (sequence, (Date().timeIntervalSince1970 * 1000).rounded(.down))
            } else {
                guard step.name == "skip_prev", previousCueReady(cue) else { return }
                // Mark the second write before suspension; a query, clock, or
                // duplicate ACK must never send this skip again.
                inFlight?.previousCue?.previousSent = true
            }
        }
        previousQueueRefresh = nil
        if step.name == "skip_prev", let cue = command.previousCue, let media {
            previousQueueRefresh = PreviousQueueRefresh(id: command.request.id, sequence: sequence, epoch: queueEpoch,
                                                        media: media, context: cue.context,
                                                        deadline: Double(command.request.expiresAtUptimeMs) / 1000)
        }
        if command.request.control.kind == "autoplay_enqueue" {
            // A timeout is inconclusive. Never send a second automatic write for
            // the same queue epoch, even when its ACK or queue update is delayed.
            autoplayAttemptedEpoch = queueEpoch
        }
        if (step.name == "skip_next" || step.name == "skip_prev"), let media, let anchor {
            let now = ProcessInfo.processInfo.systemUptime
            let position = anchor.position + max(0, now - anchor.uptime) * anchor.rate * 1000
            let duplicate = if step.name == "skip_next", let queuePrefix,
                now - queuePrefix.observedAt <= 1.2, queuePrefix.uniqueHead?.media == media,
                let repeatMode = options?["repeat"] as? String, repeatMode != "one" {
                DuplicateNext(wireID: command.request.id, queue: queuePrefix, epoch: queueEpoch, context: queueContext)
            } else { nil as DuplicateNext? }
            traceNextReset(.superseded)
            if step.name == "skip_next" { traceNextArming(media: media, hasAnchor: true, armed: duplicate != nil, now: now) }
            relativeTransition = RelativeTransition(sentAt: now, media: media, position: position,
                                                    canRestart: step.name == "skip_prev", duplicateNext: duplicate)
        } else {
            traceNextReset(.superseded)
            if step.name == "skip_next" {
                traceNextArming(media: media, hasAnchor: anchor != nil, armed: false, now: ProcessInfo.processInfo.systemUptime)
            }
            relativeTransition = nil
        }
        var fields = step.fields
        fields["type"] = "command"; fields["command"] = step.name
        do {
            let data = try JSONSerialization.data(withJSONObject: fields)
            traceCommand(.sending, command.request.control)
            try await connection.send(.string(String(decoding: data, as: UTF8.self)))
        } catch { failCommand(id: command.request.id, connection: connection) }
    }
    private func receiveControlResult(_ message: [String: Any], connection: URLSessionWebSocketTask) async -> Bool {
        if let pending = activation, pending.connection === connection {
            if message["type"] as? String == "command_result", message["command"] as? String == "activate" {
                if ActivationClockPolicy.timelyReceipt(continuous: clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW), deadline: pending.receiptDeadline) {
                    finishActivation(accepted: true)
                } else {
                    retireActivation(id: pending.request.id, connection: connection)
                }
                return true
            }
            if message["type"] as? String == "auth_state", boolean(message["logged_in"]) == false {
                retireActivation(id: pending.request.id, connection: connection)
            }
        }
        if message["type"] as? String == "command_result", message["command"] as? String == "play", playAckFence != nil {
            playAckFence = nil
            if Self.trace { Self.logger.info("soloist_ack_recovery stage=late_play_ack_drained") }
            return true
        }
        guard let command = inFlight else { return false }
        switch message["type"] as? String {
        case "command_result":
            guard let expected = command.remaining.first, message["command"] as? String == expected.name else { return true }
            if let cue = command.previousCue {
                guard expected.name == "seek" ? cue.seek != nil : cue.previousSent else { return true }
            }
            if Self.trace, ["start_uri", "play", "pause", "next", "previous", "previous_from_start", "seek"].contains(command.request.control.kind) {
                Self.logger.info("soloist_wire_ack wire=\(command.request.id, privacy: .public) kind=\(command.request.control.kind, privacy: .public) accepted=true")
            }
            inFlight?.remaining.removeFirst()
            if inFlight?.remaining.isEmpty == true {
                if ["start_uri", "enqueue", "autoplay_enqueue", "next", "previous", "shuffle", "repeat"].contains(command.request.control.kind) {
                    nextQueueAt = 0
                }
                finishControl(accepted: true)
                if command.previousCue != nil {
                    await refreshQueue(connection, now: ProcessInfo.processInfo.systemUptime, onlyAfterPrevious: true)
                }
                await refresh(connection)
            } else if command.previousCue != nil {
                await advancePreviousCue(connection)
                await refreshPreviousCue(connection, force: true)
            } else {
                await sendStep(connection)
            }
            return true
        case "error":
            // Errors have no command identity and a previous read can still reply.
            // Wait for the matching acknowledgement or the existing timeout.
            return true
        case "auth_state":
            guard let authenticated = boolean(message["logged_in"]) else { return true }
            if !authenticated || boolean(message["is_active"]) == false || command.previousCue != nil {
                failCommand(id: command.request.id, connection: connection)
                return false
            }
            // A positive announcement must not clear the host's pending control.
            return true
        case "device_changed":
            if boolean(message["is_active"]) == false {
                failCommand(id: command.request.id, connection: connection)
            }
            return false
        default: return false
        }
    }
    private func atPreviousCue(_ state: String, at now: Double) -> Bool {
        guard status == state, let anchor else { return false }
        if state == "paused" { return anchor.position == 0 && anchor.rate == 0 }
        return anchor.rate > 0
            && anchor.position + max(0, now - anchor.uptime) * anchor.rate * 1000 <= Self.previousCueMaxPosition
    }
    private func observePreviousCue(position raw: Any?, at now: Double) {
        guard var cue = inFlight?.previousCue, let seek = cue.seek, !cue.previousSent else { return }
        let timestamp = number((raw as? [String: Any])?["timestamp_ms"])
        let confirmed = timestamp.map { $0 >= seek.wallMs } == true
            && cue.context == queueContext && atPreviousCue(cue.state, at: now)
            && (actions?["previous"] as? Bool) == true
        cue.confirmedAt = confirmed ? now : nil
        inFlight?.previousCue = cue
    }
    private func previousCueReady(_ cue: PreviousCue) -> Bool {
        let now = ProcessInfo.processInfo.systemUptime
        return !cue.previousSent && cue.seek.map { sequence > $0.sequence } == true
            && cue.confirmedAt.map { now - $0 <= 0.18 } == true
            && atPreviousCue(cue.state, at: now) && (actions?["previous"] as? Bool) == true
    }
    private func advancePreviousCue(_ connection: URLSessionWebSocketTask) async {
        guard socket === connection, let command = inFlight, let cue = command.previousCue, !cue.previousSent else { return }
        guard sessionEligible(command.request), matchesRequiredOccurrence(command.request), cue.context == queueContext,
              status == cue.state || status == "buffering" else {
            failCommand(id: command.request.id, connection: connection); return
        }
        if command.remaining.first?.name == "skip_prev", previousCueReady(cue) { await sendStep(connection) }
    }
    private func refreshPreviousCue(_ connection: URLSessionWebSocketTask, force: Bool = false) async {
        let now = ProcessInfo.processInfo.systemUptime
        guard socket === connection, let command = inFlight, command.previousCue?.previousSent == false,
              command.remaining.first?.name == "skip_prev", sessionEligible(command.request),
              force || now - lastQueryAt >= 0.1 else { return }
        lastQueryAt = now
        try? await connection.send(.string("{\"type\":\"command\",\"command\":\"get_state\"}"))
    }
    private func failCommand(id: UInt64, connection: URLSessionWebSocketTask, timeout: Bool = false) {
        guard socket === connection, inFlight?.request.id == id else { return }
        retireConnection(connection, recovery: timeout ? playbackRecovery() : nil, timeout: timeout)
    }
    private func playbackRecovery(_ witness: PlaybackWitness? = nil) -> SocketRecovery? {
        guard loggedIn, active, let playback = witness ?? inFlight?.playback,
              let observedAt = playback.observedAt, let port = socketPort,
              playback.epoch == queueEpoch, let anchor,
              playback.matches(media: media, context: queueContext, state: status, rate: anchor.rate) else { return nil }
        let now = ProcessInfo.processInfo.systemUptime
        let deadline = min(now + 0.30, observedAt + 0.75)
        return now < deadline ? SocketRecovery(playback: playback, port: port, deadline: deadline) : nil
    }
    private func finishPlayAfterGrace(id: UInt64, connection: URLSessionWebSocketTask) {
        guard socket === connection, let command = inFlight, command.request.id == id,
              playbackRecovery() != nil else { return }
        playAckFence = command.ackDeadline
        if Self.trace { Self.logger.info("soloist_ack_recovery stage=play_proven_waiting_for_ack") }
        finishControl(accepted: true)
    }
    private func retireConnection(_ connection: URLSessionWebSocketTask, recovery: SocketRecovery?, timeout: Bool) {
        // The public ACK carries a command name only. Retire this socket so a
        // delayed ACK cannot complete the next invocation of the same command.
        socketRecovery = recovery
        if Self.trace, let recovery {
            Self.logger.info("soloist_ack_recovery stage=retired state=\(recovery.playback.state, privacy: .public) trigger=timeout")
        }
        socket = nil
        connection.cancel(with: .goingAway, reason: nil)
        finishControl(accepted: recovery != nil, timeout: timeout)
    }
    private func finishControl(accepted: Bool, timeout: Bool = false) {
        guard let command = inFlight else { return }
        inFlight = nil; controlTimeout?.cancel(); controlTimeout = nil
        playAckGrace?.cancel(); playAckGrace = nil
        if previousQueueRefresh?.id == command.request.id {
            if accepted { previousQueueRefresh?.acknowledged = true }
            else { previousQueueRefresh = nil }
        }
        if !accepted && command.request.control.kind == "repeat" { repeatTransition = nil }
        if Self.trace, command.request.control.kind == "next" {
            let witness = relativeTransition?.duplicateNext
            let matches = witness?.wireID == command.request.id
            Self.logger.info("soloist_same_next stage=ack accepted=\(accepted, privacy: .public) timeout=\(timeout, privacy: .public) matching_witness=\(matches, privacy: .public) queue_advanced=\(witness?.queueAdvanced == true, privacy: .public) buffered=\(witness?.observedBuffering == true, privacy: .public) playing=\(witness?.confirmedPlaying == true, privacy: .public)")
        }
        if command.request.control.kind == "next", relativeTransition?.duplicateNext?.wireID == command.request.id {
            if accepted { relativeTransition?.duplicateNext?.acknowledged = true }
            else { relativeTransition?.duplicateNext = nil }
        }
        traceCommand(accepted ? .accepted : timeout ? .timeout : .rejected, command.request.control)
        emit(["event":"control_result", "id":command.request.id, "accepted":accepted])
        if let wireID = confirmDuplicateNext() { emitClock(confirmedSameMediaNext: wireID) }
    }
    private func refresh(_ connection: URLSessionWebSocketTask) async {
        guard socket === connection, socketRecovery == nil else { return }
        let now = ProcessInfo.processInfo.systemUptime
        if let transition = relativeTransition, now >= transition.deadline {
            if transition.incomplete {
                tracePlayback(.clockCleared, .expired, status: status, hasMedia: media != nil, hasPosition: anchor != nil, active: active)
            }
            traceNextReset(.expired)
            relativeTransition = nil
            if transition.incomplete { clearIncompletePlayback() }
        }
        if (loggedIn || !receivedAuth), now - lastStateAt > 6 {
            connection.cancel(with: .goingAway, reason: nil); return
        }
        guard loggedIn else { return }
        if inFlight != nil && inFlight?.playback == nil {
            if inFlight?.previousCue != nil { await refreshPreviousCue(connection) }
            return
        }
        if now - lastQueryAt >= 0.1 {
            lastQueryAt = now
            try? await connection.send(.string("{\"type\":\"command\",\"command\":\"get_state\"}"))
        }
        await refreshQueue(connection, now: ProcessInfo.processInfo.systemUptime)
    }
    private func invalidateQueue(_ reason: NextScopeReset, mediaChanged: Bool = false,
                                 contextChanged: Bool = false, activeChanged: Bool = false) {
        traceNextReset(reason, mediaChanged: mediaChanged, contextChanged: contextChanged, activeChanged: activeChanged)
        if previousQueueRefresh?.successor != nil || activeChanged || [.auth, .socket, .stateCleared, .playbackIdle].contains(reason) {
            previousQueueRefresh = nil
        }
        queueEpoch &+= 1
        queuePrefix = nil
        queueReady = false
        nextQueueAt = 0
        nextTailQueueAt = 0
        emptyTail = nil
    }
    private func duplicateNextScope(_ transition: RelativeTransition, _ witness: DuplicateNext) -> Bool {
        // Past the provider's transport settle window, a new command may already be pending.
        active && media == transition.media && witness.context == queueContext && witness.epoch == queueEpoch
            && ProcessInfo.processInfo.systemUptime - transition.sentAt < 0.45
    }
    private func confirmDuplicateNext() -> UInt64? {
        guard let transition = relativeTransition, let witness = transition.duplicateNext,
              duplicateNextScope(transition, witness), witness.acknowledged, witness.queueAdvanced,
              witness.confirmedPlaying, status == "playing", actions != nil,
              let repeatMode = options?["repeat"] as? String, repeatMode != "one" else { return nil }
        // A verified second occurrence has the same URI but a new transport boundary.
        relativeTransition = nil
        minimumControlSequence = sequence &+ 1
        invalidateQueue(.confirmed)
        queueReady = true
        return witness.wireID
    }
    private func observeDuplicateNext(state: String, position: Double, rate: Double) -> UInt64? {
        guard let transition = relativeTransition, let witness = transition.duplicateNext,
              duplicateNextScope(transition, witness), let repeatMode = options?["repeat"] as? String,
              repeatMode != "one" else {
            traceNextSnapshot(state: state, full: true, sameMedia: relativeTransition?.media == media,
                              isActive: active, position: position, rate: rate)
            return nil
        }
        if state == "buffering", rate == 0, position <= 1_000 {
            relativeTransition?.duplicateNext?.observedBuffering = true
        }
        relativeTransition?.duplicateNext?.confirmedPlaying = witness.observedBuffering
            && state == "playing" && rate > 0 && position <= 1_500 && actions != nil
        traceNextSnapshot(state: state, full: true, sameMedia: true, isActive: active, position: position, rate: rate)
        return confirmDuplicateNext()
    }
    private func autoplayRemaining(at now: Double) -> Double? {
        guard status == "playing", let anchor, anchor.rate > 0,
              identifier(media, kinds: ["track"]) != nil,
              options?["repeat"] as? String == "off",
              let duration = number(metadata?["duration_ms"]), duration > 0 else { return nil }
        return duration - (anchor.position + max(0, now - anchor.uptime) * anchor.rate * 1000)
    }
    private func autoplayTailReady(at now: Double) -> Bool {
        guard queueReady, autoplayAttemptedEpoch != queueEpoch,
              let emptyTail, emptyTail.epoch == queueEpoch else { return false }
        return emptyTail.observedAt - emptyTail.since >= 0.2 && now - emptyTail.observedAt <= 1.2
    }
    private func canReadCuedPreviousQueue(state: String, rate: Double) -> Bool {
        (state == "playing" && rate > 0) || ((state == "paused" || state == "buffering") && rate == 0)
    }
    private func refreshQueue(_ connection: URLSessionWebSocketTask, now: Double, onlyAfterPrevious: Bool = false) async {
        if let previousQueueRefresh, now >= previousQueueRefresh.deadline { self.previousQueueRefresh = nil }
        let afterPrevious: Bool
        if let previous = previousQueueRefresh {
            // A query issued against the outgoing track can still be in flight.
            // Wait for the successor and preserve the usual pending-request guard.
            guard previous.acknowledged, let successor = previous.successor,
                  successor.media == media, successor.epoch == queueEpoch, previous.context == queueContext,
                  (actions?["next"] as? Bool) == true, let anchor,
                  canReadCuedPreviousQueue(state: status, rate: anchor.rate) else { return }
            afterPrevious = true
        } else {
            afterPrevious = false
        }
        if onlyAfterPrevious && !afterPrevious { return }
        let approachingTail = autoplayAttemptedEpoch != queueEpoch
            && autoplayRemaining(at: now).map { (1...3000).contains($0) } == true
        guard socket === connection, loggedIn, active, queueReady, inFlight == nil,
              now - lastStateAt <= 0.9, now >= nextQueueAt || approachingTail && now >= nextTailQueueAt,
              queueRequest.map({ now >= $0.expires }) ?? true else { return }
        queueRequest = (queueEpoch, now + 1)
        nextQueueAt = now + 2
        nextTailQueueAt = now + 0.5
        if afterPrevious {
            previousQueueRefresh = nil
            if Self.trace { Self.logger.info("soloist_queue stage=request_after_cued_previous") }
        }
        try? await connection.send(.string("{\"type\":\"command\",\"command\":\"get_queue\",\"limit\":10}"))
    }
    private func receiveQueue(_ message: [String: Any]) {
        let requested = queueRequest
        queueRequest = nil
        guard requested == nil || requested?.epoch == queueEpoch else {
            traceNextQueueRejection(.requestEpoch); return
        }
        guard loggedIn, active, queueReady, let media else {
            traceNextQueueRejection(.unavailable); return
        }
        guard let previous = message["previous"] as? [Any], let upcoming = message["upcoming"] as? [Any] else {
            traceNextQueueRejection(.missingRows); return
        }
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastStateAt <= 0.9 else {
            traceNextQueueRejection(.stateStale); return
        }
        if previousQueueRefresh?.successor?.epoch == queueEpoch { previousQueueRefresh = nil }
        let prefix = upcoming.prefix(QueuePrefix.limit).compactMap { raw -> QueueOccurrence? in
            guard let row = raw as? [String: Any], let uid = row["uid"] as? String,
                  !uid.isEmpty, uid.utf8.count <= 256, let item = row["item"] as? [String: Any],
                  let uri = identifier(item["uri"]), let source = row["source"] as? String,
                  ["context", "queue", "autoplay"].contains(source) else { return nil }
            return QueueOccurrence(uid: uid, media: uri, source: source)
        }
        let distinct = Self.trace ? Set(prefix.map(\.uid)).count : 0
        let observed = QueuePrefix(rows: prefix, count: upcoming.count, observedAt: now)
        if let transition = relativeTransition, let witness = transition.duplicateNext,
           duplicateNextScope(transition, witness) {
            relativeTransition?.duplicateNext?.queueAdvanced = observed.map { witness.queue.removesHead(in: $0) } == true
            traceNextQueue(observed, count: upcoming.count, parsed: prefix.count, distinct: distinct, requested: requested != nil,
                           advanced: relativeTransition?.duplicateNext?.queueAdvanced == true, now: now)
            if let wireID = confirmDuplicateNext() { emitClock(confirmedSameMediaNext: wireID) }
        } else {
            traceNextQueue(observed, count: upcoming.count, parsed: prefix.count, distinct: distinct,
                           requested: requested != nil, advanced: false, now: now)
        }
        queuePrefix = observed
        // Keep the raw count: an unrecognized queue item still belongs to the
        // device and must not be displaced by an Auvra recommendation.
        if !upcoming.isEmpty {
            emptyTail = nil
        } else if requested?.epoch == queueEpoch {
            let prior = emptyTail.flatMap { $0.epoch == queueEpoch && now - $0.observedAt <= 1.2 ? $0.since : nil }
            emptyTail = (queueEpoch, prior ?? now, now)
        }
        func rows(_ values: [Any]) -> [[String: Any]] {
            values.prefix(10).compactMap { raw in
                guard let row = raw as? [String: Any], let item = row["item"] as? [String: Any],
                      let uri = identifier(item["uri"]) else { return nil }
                let source = (row["source"] as? String).flatMap { ["context", "queue", "autoplay"].contains($0) ? $0 : nil }
                return ["media_id": uri, "source": source as Any? ?? NSNull(),
                        "metadata": itemMetadata(item, context: nil, requireDuration: false) as Any? ?? NSNull()]
            }
        }
        queueSequence &+= 1
        // Keep the unique same-track head fresh before a possible Next.
        // Reads stay off the command path and ordinary queues keep their cadence.
        let refreshSameMediaHead = observed?.uniqueHead?.media == media
            && (options?["repeat"] as? String).map { $0 != "one" } == true
        nextQueueAt = now + (refreshSameMediaHead ? 0.65 : 12)
        var queue: [String: Any] = ["epoch":queueEpoch, "sequence":queueSequence, "media_id":media, "uptime":now,
                                    "previous":rows(previous), "upcoming":rows(upcoming)]
        if autoplayTailReady(at: now) { queue["autoplay_ready"] = true }
        emit(["event":"queue", "queue":queue])
    }
    private func number(_ value: Any?) -> Double? {
        guard let value = value as? NSNumber, CFGetTypeID(value) != CFBooleanGetTypeID(), value.doubleValue.isFinite else { return nil }
        return value.doubleValue
    }
    private func boolean(_ value: Any?) -> Bool? {
        guard let value = value as? NSNumber, CFGetTypeID(value) == CFBooleanGetTypeID() else { return nil }
        return value.boolValue
    }
    private func identifier(_ value: Any?, kinds: [String] = ["track", "episode", "chapter"]) -> String? {
        guard let value = value as? String else { return nil }
        let parts = value.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "spotify", kinds.contains(String(parts[1])),
              parts[2].utf8.count == 22, parts[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) else { return nil }
        return value
    }
    private func text(_ value: Any?, maxBytes: Int) -> String? {
        guard let value = value as? String, value.utf8.count <= maxBytes,
              !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else { return nil }
        let clean = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return clean.isEmpty ? nil : clean
    }
    private func cover(_ decorations: [String: Any]?) -> String? {
        let visual = decorations?["visual_identity"] as? [String: Any]
        let covers = visual?["cover"] as? [[String: Any]] ?? []
        return covers.prefix(8).compactMap { image -> (String, Int)? in
            guard let value = text(image["url"], maxBytes: 512), !value.contains("\\"),
                  let url = URLComponents(string: value), url.scheme == "https", url.user == nil,
                  url.password == nil, url.port == nil, url.query == nil, url.fragment == nil,
                  let host = url.host, !url.path.isEmpty else { return nil }
            let standard = ["i.scdn.co", "mosaic.scdn.co"].contains(host)
            let custom = ["image-cdn-fa.spotifycdn.com", "image-cdn-ak.spotifycdn.com"].contains(host) && url.path.hasPrefix("/image/")
            guard standard || custom else { return nil }
            let rank = ["small", "default", "large", "xlarge"].firstIndex(of: image["size"] as? String ?? "") ?? 0
            return (value, rank)
        }.max(by: { $0.1 < $1.1 })?.0
    }
    private func itemMetadata(_ item: [String: Any]?, context: String?, requireDuration: Bool = true) -> [String: Any]? {
        guard let decorations = item?["decorations"] as? [String: Any],
              let title = text((decorations["identity"] as? [String: Any])?["name"], maxBytes: 384) else { return nil }
        let duration = number((decorations["playback"] as? [String: Any])?["duration_ms"])
            .flatMap { (1...86_400_000).contains($0) && $0.rounded() == $0 ? UInt64($0) : nil }
        guard duration != nil || !requireDuration else { return nil }
        let parent = (decorations["parent"] as? [String: Any])?["entity"] as? [String: Any]
        let parentDecorations = parent?["decorations"] as? [String: Any]
        let album = text((parentDecorations?["identity"] as? [String: Any])?["name"], maxBytes: 256)
        let creators = decorations["creators"] as? [[String: Any]] ?? []
        let artists = creators.prefix(6).compactMap { creator in
            let entity = creator["entity"] as? [String: Any]
            let info = entity?["decorations"] as? [String: Any]
            return text((info?["identity"] as? [String: Any])?["name"], maxBytes: 128)
        }
        let value: [String: Any] = ["title": title, "artists": artists, "album": album as Any? ?? NSNull(),
                                    "duration_ms": duration ?? 0, "artwork": (cover(decorations) ?? cover(parentDecorations)) as Any? ?? NSNull(),
                                    "context_uri": context as Any? ?? NSNull()]
        guard let data = try? JSONSerialization.data(withJSONObject: value), data.count <= 2048 else { return nil }
        return value
    }
    private func contextURI(_ message: [String: Any]) -> String? {
        identifier((message["context"] as? [String: Any])?["uri"], kinds: ["album", "artist", "playlist", "show", "audiobook", "track", "episode", "chapter"])
    }
    private func position(_ value: Any?) -> PositionAnchor? {
        guard let value = value as? [String: Any], let ms = number(value["position_ms"]), (0...86_400_000).contains(ms),
              let timestamp = number(value["timestamp_ms"]), timestamp >= 0,
              let speed = number(value["speed"]), (0...4).contains(speed) else { return nil }
        let age = Date().timeIntervalSince1970 * 1000 - timestamp
        guard age >= -5000, age <= 86_400_000 else { return nil }
        return .init(position: min(86_400_000, ms + max(0, age) * speed),
                     uptime: ProcessInfo.processInfo.systemUptime, rate: speed, sourceTimestampMs: timestamp)
    }
    private func playbackOptions(_ value: Any?) -> [String: Any]? {
        guard let value = value as? [String: Any], let shuffle = boolean(value["shuffle"]),
              let mode = value["repeat"] as? String, ["off","context","track"].contains(mode) else { return nil }
        let reportedMode = (value["modes"] as? [String: Any])?["context_enhancement"] as? String
        let enhancement: String? = reportedMode == "RECOMMENDATION" ? "recommendations" : reportedMode == "NONE" ? "off" : nil
        var repeatMode = mode == "context" ? "all" : mode == "track" ? "one" : "off"
        if let transition = repeatTransition {
            if repeatMode == transition.requested || ProcessInfo.processInfo.systemUptime >= transition.expires {
                repeatTransition = nil
            } else {
                // The two Soloist writes form one user intent. Publish the old
                // mode until the expected mode is observed or the window expires.
                repeatMode = transition.previous
            }
        }
        return ["shuffle":shuffle,"repeat":repeatMode, "enhancement":enhancement as Any? ?? NSNull()]
    }
    private func playbackActions(_ value: Any?) -> [String: Any]? {
        guard let value = value as? [String: Any], value.count <= 32 else { return nil }
        let fields = ["play":"play","pause":"pause","seek":"seek","next":"skip_next","previous":"skip_prev","shuffle":"shuffle","repeat":"set_repeat","queue_add":"add_to_queue"]
        let known = Array(fields.values) + ["seek_forward","seek_backward"]
        guard known.allSatisfy({ value[$0] == nil || value[$0] is [String: Any] }) else { return nil }
        var result: [String: Any] = [:]
        for (field, action) in fields { result[field] = value[action] is [String: Any] }
        for direction in ["forward","backward"] {
            let step = (value["seek_" + direction] as? [String: Any])?["step_ms"]
            if let ms = number(step), (1...300_000).contains(ms), ms.rounded() == ms { result["seek_" + direction + "_ms"] = UInt64(ms) }
        }
        return result
    }
    private func receive(_ message: [String: Any]) -> Bool {
        var needsState = false
        var confirmedSameMediaNext: UInt64?
        switch message["type"] as? String {
        case "queue_changed": receiveQueue(message); return false
        case "auth_state":
            guard let value = boolean(message["logged_in"]) else { return false }
            tracePlayback(.clockCleared, .auth, status: status, hasMedia: media != nil, hasPosition: anchor != nil, active: active)
            receivedAuth = true
            minimumControlSequence = sequence &+ 1
            minimumSessionControlSequence = minimumControlSequence
            lastStateAt = ProcessInfo.processInfo.systemUptime
            lastStateContinuousNs = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
            loggedIn = value; active = boolean(message["is_active"]) ?? false
            queueRequest = nil; queueContext = nil; invalidateQueue(.auth)
            emit(["event":"auth","logged_in":value])
            media = nil; anchor = nil; previousTrack = nil; status = "idle"; emit(["event":"clock","clock":NSNull()])
            volume = nil; options = nil; actions = nil; metadata = nil; repeatTransition = nil; relativeTransition = nil
            return value
        case "playback_state":
            guard loggedIn, let value = message["status"] as? String, ["idle","playing","paused","buffering"].contains(value),
                  let isActive = boolean(message["is_active"]) else { return false }
            let item = message["item"] as? [String: Any]
            let nextMedia = identifier(item?["uri"])
            let nextAnchor = position(message["position"])
            let now = ProcessInfo.processInfo.systemUptime
            // A get_state issued before track_changed can still return the prior item.
            if isActive, value != "idle", let previousTrack, nextMedia == previousTrack.uri, now < previousTrack.until { return false }
            if !isActive || value == "idle" || nextMedia != media { previousTrack = nil }
            guard value == "idle" || nextMedia != nil, let next = nextAnchor else {
                traceNextSnapshot(state: value, full: false, sameMedia: nextMedia != nil && nextMedia == media,
                                  isActive: isActive, position: nextAnchor?.position, rate: nextAnchor?.rate)
                let rawItem = message["item"]
                let rawURI = item?["uri"]
                if nextMedia == nil { traceItemShape(rawItem, rawURI) }
                let itemOmitted = rawItem == nil || rawItem is NSNull
                    || (item != nil && (rawURI == nil || rawURI is NSNull))
                let canHoldBuffering = value == "buffering" && (nextMedia != nil || itemOmitted)
                let canHoldUnidentifiedPlaying = value == "playing" && nextAnchor != nil
                let emptyURI = canHoldUnidentifiedPlaying && (rawURI as? String)?.isEmpty == true
                if canHoldBuffering || canHoldUnidentifiedPlaying,
                   active, isActive, var transition = relativeTransition,
                   now < (emptyURI ? transition.deadline : transition.standardDeadline),
                   media != nil, let anchor {
                    // Relative navigation can report Playing before it identifies
                    // the current item. Discard this item and position. Retain the
                    // outgoing identity at zero rate until the complete snapshot.
                    if !transition.incomplete {
                        invalidateQueue(.incomplete)
                        tracePlayback(.incompleteHeld, .activeIncomplete, status: value, hasMedia: nextMedia != nil, hasPosition: nextAnchor != nil, active: isActive)
                    }
                    transition.incomplete = true
                    // This exact Soloist placeholder has been observed through
                    // longer relative transitions. Its deadline never renews.
                    transition.observedEmptyURI = emptyURI
                    relativeTransition = transition
                    status = "buffering"
                    self.anchor = .init(position: anchor.position, uptime: now, rate: 0)
                    actions = nil; options = nil; metadata = nil
                    emitClock()
                    return true
                }
                if !isActive {
                    active = false
                    minimumSessionControlSequence = sequence &+ 1
                    minimumControlSequence = minimumSessionControlSequence
                    if let connection = socket, let request = inFlight?.request {
                        failCommand(id: request.id, connection: connection)
                    }
                }
                tracePlayback(.clockCleared, isActive ? .invalid : .inactive, status: value, hasMedia: nextMedia != nil, hasPosition: nextAnchor != nil, active: isActive)
                clearIncompletePlayback()
                return false
            }
            if let transition = relativeTransition {
                let previousRestarted = transition.canRestart && next.position + 500 < transition.position
                if !isActive || nextMedia != transition.media || value == "idle" || previousRestarted || now >= transition.deadline {
                    traceNextReset(.fullState, mediaChanged: nextMedia != transition.media, activeChanged: isActive != active)
                    relativeTransition = nil
                } else if transition.incomplete {
                    // A delayed full snapshot of the outgoing item cannot
                    // restore its actions after incomplete buffering began.
                    lastStateAt = now
                    lastStateContinuousNs = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
                    tracePlayback(.previousSnapshot, .activeIncomplete, status: value, hasMedia: true, hasPosition: true, active: true)
                    return true
                }
            }
            if nextMedia != media || value == "idle" {
                metadata = nil; repeatTransition = nil; minimumControlSequence = sequence &+ 1
            }
            let context = contextURI(message)
            if nextMedia != media || isActive != active || context != queueContext {
                invalidateQueue(.fullState, mediaChanged: nextMedia != media, contextChanged: context != queueContext, activeChanged: isActive != active)
            }
            if isActive != active { minimumSessionControlSequence = sequence &+ 1 }
            queueContext = context
            queueReady = isActive && nextMedia != nil && value != "idle"
            if nextMedia != nil && value != "idle" {
                if let next = itemMetadata(item, context: context) { metadata = next }
                else { metadata?["context_uri"] = context as Any? ?? NSNull() }
            }
            status = value; active = isActive; media = nextMedia; anchor = next
            lastStateAt = now
            lastStateContinuousNs = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
            if let command = inFlight, let playback = command.playback {
                let confirmed = isActive && playback.epoch == queueEpoch
                    && sequence &+ 1 > playback.sequence
                    && playback.matches(media: nextMedia, context: context, state: value, rate: next.rate)
                inFlight?.playback?.observedAt = confirmed ? now : nil
                if confirmed, command.request.control.kind == "play", playAckGrace == nil, let connection = socket {
                    let id = command.request.id
                    playAckGrace = Task { [weak self] in
                        try? await Task.sleep(for: .milliseconds(120))
                        guard !Task.isCancelled else { return }
                        await self?.finishPlayAfterGrace(id: id, connection: connection)
                    }
                } else if !confirmed {
                    playAckGrace?.cancel()
                }
            }
            failures = 0
            volume = number(message["volume"]).flatMap { (0...100).contains($0) ? $0 / 100 : nil }
            options = playbackOptions(message["options"])
            actions = value == "buffering" && relativeTransition != nil ? nil : playbackActions(message["available_actions"])
            if let previous = previousQueueRefresh {
                if !isActive || context != previous.context || value == "idle" || now >= previous.deadline {
                    previousQueueRefresh = nil
                } else if let nextMedia, nextMedia != previous.media, queueEpoch > previous.epoch,
                          sequence &+ 1 > previous.sequence, let nextAllowed = actions?["next"] as? Bool,
                          canReadCuedPreviousQueue(state: value, rate: next.rate) {
                    if nextAllowed { previousQueueRefresh?.successor = (nextMedia, queueEpoch) }
                    else { previousQueueRefresh = nil }
                }
            }
            observePreviousCue(position: message["position"], at: now)
            confirmedSameMediaNext = observeDuplicateNext(state: value, position: next.position, rate: next.rate)
        case "position_sync":
            guard loggedIn, media != nil, let next = position(message["position"]) else { return false }
            anchor = next
        case "track_changed":
            guard loggedIn, let item = message["item"] as? [String: Any] else { return false }
            invalidateQueue(.trackChanged)
            let nextMedia = identifier(item["uri"])
            if nextMedia != media {
                let now = ProcessInfo.processInfo.systemUptime
                previousTrack = media.map { ($0, now + 0.9) }
                media = nextMedia; metadata = nil; queueContext = nil
                repeatTransition = nil; minimumControlSequence = sequence &+ 1
                status = nextMedia == nil ? "idle" : "buffering"
                anchor = .init(position: 0, uptime: now, rate: 0)
            }
            if nextMedia != nil, let next = itemMetadata(item, context: queueContext) { metadata = next }
            options = nil; actions = nil; needsState = true
        case "playback_changed":
            guard loggedIn, let value = message["status"] as? String, ["idle","playing","paused","buffering"].contains(value) else { return false }
            if value == "idle" { invalidateQueue(.playbackIdle) }
            if value != "buffering", relativeTransition?.incomplete == true {
                return true // Only a complete snapshot can end this uncertain transition.
            }
            if let old = anchor {
                let now = ProcessInfo.processInfo.systemUptime
                let presented = old.position + (status == "playing" ? max(0, now - old.uptime) * old.rate * 1000 : 0)
                anchor = .init(position: min(86_400_000, presented), uptime: now, rate: value == "playing" ? old.rate : 0)
            }
            status = value
            actions = nil; needsState = true
        case "device_changed":
            guard loggedIn, let value = boolean(message["is_active"]) else { return false }
            if value != active {
                invalidateQueue(.deviceChanged, activeChanged: true); actions = nil; minimumSessionControlSequence = sequence &+ 1
            }
            active = value
            if !value { previousTrack = nil; repeatTransition = nil; relativeTransition = nil; minimumControlSequence = sequence &+ 1 }
            needsState = value
        case "volume_changed":
            guard loggedIn, let value = number(message["volume"]), (0...100).contains(value) else { return false }
            volume = value / 100
        case "options_changed":
            guard loggedIn else { return false }
            options = playbackOptions(message["options"]); actions = nil; needsState = true
        case "context_changed":
            guard loggedIn else { return false }
            repeatTransition = nil
            let context = contextURI(message)
            if let previousQueueRefresh, context != previousQueueRefresh.context { self.previousQueueRefresh = nil }
            if context != queueContext { invalidateQueue(.contextChanged, contextChanged: true) }
            queueContext = context
            metadata?["context_uri"] = context as Any? ?? NSNull()
            options = nil; actions = nil; needsState = true
        default: return false
        }
        if playAckGrace != nil && playbackRecovery() == nil {
            playAckGrace?.cancel()
        }
        emitClock(confirmedSameMediaNext: confirmedSameMediaNext)
        return needsState
    }
    private func clearIncompletePlayback() {
        traceNextReset(.stateCleared)
        relativeTransition = nil
        media = nil; anchor = nil; metadata = nil; options = nil; actions = nil
        invalidateQueue(.stateCleared)
        emit(["event":"clock","clock":NSNull()])
    }
    private func emitClock(confirmedSameMediaNext: UInt64? = nil) {
        guard loggedIn, let anchor else { return }
        let now = ProcessInfo.processInfo.systemUptime
        let rate = status == "playing" ? anchor.rate : 0
        let presented = min(86_400_000, anchor.position + max(0, now - anchor.uptime) * rate * 1000)
        sequence &+= 1
        if isPausedForScopedPlay {
            if scopedPlayPausedSince == nil { scopedPlayPausedSince = sequence }
        } else {
            scopedPlayPausedSince = nil
        }
        var clock: [String: Any] = ["sequence":sequence,"media_id":media as Any? ?? NSNull(),"state":status == "buffering" ? "loading" : status,
                                   "position_ms":UInt64(presented),"rate":rate,"active":active,"uptime":now,
                                   "volume":volume as Any? ?? NSNull(),"options":options as Any? ?? NSNull(),"actions":actions as Any? ?? NSNull(),
                                   "metadata":metadata as Any? ?? NSNull(), "queue_epoch":queueEpoch]
        if let timestamp = anchor.sourceTimestampMs { clock["source_position_timestamp_ms"] = timestamp }
        if let confirmedSameMediaNext {
            if Self.trace {
                Self.logger.info("soloist_same_next stage=marker wire=\(confirmedSameMediaNext, privacy: .public) sequence=\(self.sequence, privacy: .public)")
            }
            clock["confirmed_same_media_next"] = confirmedSameMediaNext
        }
        emit(["event":"clock","clock":clock])
    }
    private func bounded(_ url: URL) -> String? {
        guard let count = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize, count <= 256,
              let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        guard let data = try? handle.read(upToCount: 257), data.count <= 256 else { return nil }
        return String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
@main
struct SoloistSupervisor {
    static func main() {
#if AUVRA_BOOTSTRAP_CLOCK_FIXTURES
        precondition(ActivationClockPolicy.freshState(now: 10.1, lastState: 10, continuous: 100_100_000_000, lastContinuous: 100_000_000_000))
        precondition(!ActivationClockPolicy.freshState(now: 10.1, lastState: 10, continuous: 200_100_000_000, lastContinuous: 100_000_000_000))
        precondition(!ActivationClockPolicy.freshState(now: 11, lastState: 10, continuous: 100_100_000_000, lastContinuous: 100_000_000_000))
        precondition(!ActivationClockPolicy.freshState(now: 10.1, lastState: 10, continuous: 100_100_000_000, lastContinuous: 0))
        precondition(!ActivationClockPolicy.freshState(now: 9, lastState: 10, continuous: 100_100_000_000, lastContinuous: 100_000_000_000))
        precondition(!ActivationClockPolicy.freshState(now: 10.1, lastState: 10, continuous: 99_000_000_000, lastContinuous: 100_000_000_000))
        precondition(ActivationClockPolicy.timelyReceipt(continuous: 100_100_000_000, deadline: 101_200_000_000))
        precondition(!ActivationClockPolicy.timelyReceipt(continuous: 200_100_000_000, deadline: 101_200_000_000))
        precondition(!ActivationClockPolicy.timelyReceipt(continuous: 100_100_000_000, deadline: 0))
        print("Bootstrap clock policy passed: nine freshness/receipt fixtures, including fresh uptime and late ACK after simulated sleep.")
#else
        signal(SIGPIPE, SIG_IGN)
        var line = Data()
        while line.count <= 16384 {
            var byte: UInt8 = 0
            guard read(STDIN_FILENO, &byte, 1) == 1 else { exit(2) }
            if byte == 10 { break }; line.append(byte)
        }
        guard line.count <= 16384, let config = try? JSONDecoder().decode(LaunchRequest.self, from: line),
              config.apiKey.utf8.count <= 8192, !config.apiKey.isEmpty,
              config.initialVolume.map({ $0 <= 100 }) != false,
              [config.executable,config.dataDirectory,config.cacheDirectory].allSatisfy({ $0.hasPrefix("/") && !$0.contains("\0") }),
              config.deviceName.utf8.count <= 100 else { exit(2) }
        line.resetBytes(in: line.startIndex..<line.endIndex)
        let child = Process(); child.executableURL = URL(fileURLWithPath: config.executable)
        child.arguments = ["--device-name",config.deviceName,"--api-key",config.apiKey,
                           "--data-dir",config.dataDirectory,"--cache-dir",config.cacheDirectory,
                           "--cache-size","256","--ws","127.0.0.1:0","--initial-volume",String(config.initialVolume ?? 40)]
        child.standardInput = FileHandle.nullDevice; child.standardOutput = FileHandle.nullDevice; child.standardError = FileHandle.nullDevice
        child.terminationHandler = { process in
            emit(["event":"exited", "code":process.terminationStatus]); exit(process.terminationStatus)
        }
        let launched = Date()
        do { try child.run() } catch { print("{\"event\":\"launch_failed\"}"); exit(3) }
        emit(["event":"started", "pid":child.processIdentifier, "clock_protocol":1, "control_protocol":1])
        let control = SoloistLocalControl()
        let directory = config.dataDirectory, ownedPID = child.processIdentifier
        // Synchronous main owns the blocking lifetime pipe. Network work must not
        // inherit main's executor.
        let connection = Task.detached { await control.connect(directory: directory, pid: ownedPID, launched: launched) }
        let requests = AsyncStream<PipeRequest>.makeStream(bufferingPolicy: .bufferingOldest(1))
        let commands = Task.detached {
            for await request in requests.stream {
                switch request {
                case .control(let request): await control.dispatch(request)
                case .activate(let request): await control.dispatchActivation(request)
                }
            }
        }
        // No other process can inherit this stdin pipe: the runtime uses /dev/null.
        var buffer = [UInt8](repeating:0,count:256)
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        var record = Data(), oversized = false
        while true {
            let count = read(STDIN_FILENO, &buffer, buffer.count)
            if count < 0 && errno == EINTR { continue }
            guard count > 0 else { break }
            for byte in buffer.prefix(count) {
                if byte == 10 {
                    if !oversized {
                        let request: PipeRequest?
                        if let header = try? decoder.decode(BootstrapHeader.self, from: record),
                           header.type == "bootstrap", header.id > 0 {
                            if let activation = try? decoder.decode(ActivationRequest.self, from: record), activation.action == "cancel" {
                                // A cancel must not wait behind the activation's
                                // async socket write. Its monotonic tombstone
                                // also rejects a still-queued older request.
                                control.recordActivationCancellation(activation.id)
                                Task.detached { await control.cancelActivation(activation.id) }
                                request = nil
                            } else if let activation = try? decoder.decode(ActivationRequest.self, from: record) {
                                request = .activate(activation)
                            } else {
                                emit(["event":"activation_result", "id":header.id, "accepted":false])
                                request = nil
                            }
                        } else {
                            request = (try? decoder.decode(ControlRequest.self, from: record)).map(PipeRequest.control)
                        }
                        if let request, case .dropped(let rejected) = requests.continuation.yield(request) {
                            switch rejected {
                            case .control(let value): emit(["event":"control_result", "id":value.id, "accepted":false])
                            case .activate(let value): emit(["event":"activation_result", "id":value.id, "accepted":false])
                            }
                        }
                    }
                    record.removeAll(keepingCapacity: true); oversized = false
                } else if record.count < 511 { record.append(byte) }
                else { oversized = true }
            }
        }
        requests.continuation.finish(); commands.cancel()
        connection.cancel()
        child.terminationHandler = nil
        if child.isRunning {
            child.terminate()
            for _ in 0..<40 { if !child.isRunning { break }; usleep(50_000) }
            if child.isRunning { kill(child.processIdentifier,SIGKILL) }
            child.waitUntilExit()
        }
#endif
    }
}
