import Foundation
import OSLog

typealias PlayerEventReceiver = @MainActor @Sendable (PlayerEvent) -> Void
typealias PlayerFailureReceiver = @MainActor @Sendable (UInt64?, String) -> Void
typealias PlayerConnectionFactory = @MainActor (@escaping PlayerEventReceiver, @escaping PlayerFailureReceiver) -> any PlayerConnection
protocol PlayerConnection: AnyObject, Sendable {
    func send(_ command: PlayerCommand, requestId: UInt64)
    func close(completion: @escaping @MainActor @Sendable () -> Void)
}

/// This owner is touched on the control queue. Its close is queued after earlier calls.
private final class PlayerHandleOwner: @unchecked Sendable {
    var handle: OpaquePointer?
    func close() { if let handle { player_destroy(handle); self.handle = nil } }
    deinit { close() }
}

/// Rust owns time and execution. Swift's serial queue only submits commands and drains events.
final class PlayerBridge: PlayerConnection, @unchecked Sendable {
    private static let playbackTiming = Logger(subsystem: "app.auvra.player", category: "PlaybackTiming")
    private static let tracePlayback = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
    private let queue = DispatchQueue(label: "app.auvra.player", qos: .userInitiated)
    private let owner = PlayerHandleOwner()
    private let timer: DispatchSourceTimer
    private let receive: @MainActor @Sendable (PlayerEvent) -> Void
    private let failure: @MainActor @Sendable (UInt64?, String) -> Void
    private var fastAnalysisPolling = false
    private var fastTransportUntil: DispatchTime?
    private var recentTransportUntil: DispatchTime?
    private var fastPolling = false
    private var failed = false
    private let admissionLock = NSLock()
    private var admitted: Set<UInt64> = []

    init(receive: @escaping @MainActor @Sendable (PlayerEvent) -> Void,
         failure: @escaping @MainActor @Sendable (UInt64?, String) -> Void) {
        self.receive = receive; self.failure = failure
        timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(250))
        timer.setEventHandler { [weak self] in self?.poll() }
        timer.resume()
    }
    deinit {
        timer.cancel()
        let owner = self.owner
        // Deinit may run on this queue when a weak timer callback releases its last reference.
        queue.async { owner.close() }
    }
    func close(completion: @escaping @MainActor @Sendable () -> Void) {
        timer.cancel()
        queue.async { [self] in
            failed = true; owner.close(); release(nil)
            DispatchQueue.main.async { completion() }
        }
    }
    func send(_ command: PlayerCommand, requestId: UInt64) {
        admissionLock.lock()
        let accepted = admitted.count < 64 && !admitted.contains(requestId)
        if accepted { admitted.insert(requestId) }
        admissionLock.unlock()
        guard accepted else { report(requestId, "The player is busy. Try the control again.", releaseAdmission: false); return }
        let beginsTransportPolling = switch command {
        case .play, .pause, .pauseConfirmedRestoredResume, .seek, .next, .previous, .skipPrevious: true
        default: false
        }
        let confirmsOwnedTransport = switch command {
        case .remote(_, .outputClock(let clock)): clock?.active == true && clock?.actions != nil
        case .remote(_, .outputCommandResult(let result)): result.accepted
        case .remote(_, .outputQueue): true
        default: false
        }
        do {
            let encoder = JSONEncoder(); encoder.keyEncodingStrategy = .convertToSnakeCase
            let data = try encoder.encode(CommandRequest(requestId: requestId, command: command))
            queue.async { [weak self] in
                guard let self else { return }
                guard let handle = self.ensureHandle() else { self.report(requestId, "The player is unavailable."); return }
                let result = data.withUnsafeBytes { bytes in
                    player_send_json(handle, bytes.baseAddress?.assumingMemoryBound(to: UInt8.self), data.count)
                }
                if Self.tracePlayback, case .next = command {
                    Self.playbackTiming.info("navigation_next_bridge request=\(requestId, privacy: .public) ffi_status=\(result, privacy: .public)")
                }
                switch result {
                case 0:
                    // Native transport may leave Rust after the first immediate drain.
                    if beginsTransportPolling {
                        let now = DispatchTime.now()
                        self.recentTransportUntil = now + .milliseconds(3500)
                        self.fastTransportUntil = now + .milliseconds(500)
                        self.updatePolling()
                    } else if confirmsOwnedTransport {
                        self.pollForTransportReply()
                    }
                case 6: self.report(requestId, "The player is busy. Try the control again.")
                case 1: self.report(requestId, "The player rejected an invalid command.")
                default: self.failed = true; self.report(nil, "The player is unavailable. Restart the player to restore your paused queue.")
                }
                self.drain(handle)
            }
        } catch { report(requestId, error.localizedDescription) }
    }
    private func ensureHandle() -> OpaquePointer? {
        if failed { return nil }
        if let handle = owner.handle { return handle }
        guard player_abi_version() == 2, player_create(&owner.handle) == 0 else {
            failed = true; report(nil, "The player could not start."); return nil
        }
        return owner.handle
    }
    private func poll() {
        updatePolling()
        guard let handle = ensureHandle() else { return }
        drain(handle)
    }
    private func updatePolling() {
        let now = DispatchTime.now()
        if let until = fastTransportUntil, now >= until { fastTransportUntil = nil }
        let fast = fastAnalysisPolling || fastTransportUntil != nil
        guard !timer.isCancelled, fast != fastPolling else { return }
        fastPolling = fast
        timer.schedule(deadline: now, repeating: .milliseconds(fast ? 33 : 250))
    }
    private func pollForTransportReply() {
        let now = DispatchTime.now()
        guard let recentTransportUntil, now < recentTransportUntil else { return }
        // A confirmation can unblock an ordered command after the initial input burst.
        // Cover one coordinator heartbeat without extending the user transport window.
        let until = min(recentTransportUntil, now + .milliseconds(150))
        guard fastTransportUntil.map({ $0 < until }) ?? true else { return }
        fastTransportUntil = until
        updatePolling()
    }
    private func drain(_ handle: OpaquePointer) {
        for _ in 0..<80 {
            var length = 0
            let status = player_event_drain(handle, nil, 0, &length)
            if status == 3 { return }
            guard status == 2, length > 0, length <= 16 * 1024 * 1024 else {
                failed = true; report(nil, "The player returned an unreadable event."); return
            }
            var data = Data(count: length)
            let result = data.withUnsafeMutableBytes { bytes in
                player_event_drain(handle, bytes.baseAddress?.assumingMemoryBound(to: UInt8.self), length, &length)
            }
            guard result == 0 else { failed = true; report(nil, "The player event could not be read."); return }
            do {
                let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
                let event = try decoder.decode(PlayerEvent.self, from: data)
                if case .commandResult(let result) = event { release(result.requestId) }
                if case .snapshot(let state) = event {
                    let fast = state.state == .playing && state.capabilities.pcmAnalysis
                    if fast != fastAnalysisPolling {
                        fastAnalysisPolling = fast
                        updatePolling()
                    }
                }
                if Self.tracePlayback, case .remoteOutputCommand(_, let command) = event {
                    if command.control.kind == .next, let request = command.requestId {
                        Self.playbackTiming.info("owned_next_bridge_received request=\(request, privacy: .public)")
                    } else if command.control.kind == .seek, let request = command.requestId {
                        Self.playbackTiming.info("owned_seek_bridge_received request=\(request, privacy: .public)")
                    } else if command.control.kind == .play, let request = command.requestId {
                        Self.playbackTiming.info("owned_play_bridge_received request=\(request, privacy: .public)")
                    } else if command.control.kind == .previousFromStart {
                        Self.playbackTiming.info("owned_previous_bridge_received id=\(command.id, privacy: .public)")
                    } else if command.control.kind == .pause {
                        Self.playbackTiming.info("owned_pause_bridge_received id=\(command.id, privacy: .public) scoped=\(command.requiredQueueEpoch != nil, privacy: .public)")
                    }
                }
                let receive = self.receive
                // Main-queue FIFO preserves event order across polling and command submission.
                DispatchQueue.main.async { receive(event) }
            } catch { failed = true; report(nil, "The player event could not be decoded: \(error.localizedDescription)"); return }
        }
    }
    private func release(_ requestId: UInt64?) {
        admissionLock.lock()
        if let requestId { admitted.remove(requestId) } else { admitted.removeAll() }
        admissionLock.unlock()
    }
    private func report(_ requestId: UInt64?, _ message: String, releaseAdmission: Bool = true) {
        if releaseAdmission { release(requestId) }
        let failure = self.failure
        DispatchQueue.main.async { failure(requestId, message) }
    }
}
