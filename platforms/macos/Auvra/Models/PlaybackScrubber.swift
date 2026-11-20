import Foundation
import Observation

/// A held seek gesture owns its preview while the backend clock keeps running.
/// After release, keep the requested position until a matching snapshot arrives.
@MainActor @Observable
final class PlaybackScrubber {
    private(set) var position: Double?
    private(set) var isEditing = false
    private var trackID: String?
    private var seekRequest: UInt64?

    func begin(at position: Double, trackID: String?) {
        guard position.isFinite, let trackID else { return }
        self.trackID = trackID
        self.position = position
        isEditing = true
        seekRequest = nil
    }

    /// Returns an immediate seek only for keyboard/accessibility edits outside a drag.
    func update(_ position: Double) -> Double? {
        guard position.isFinite else { return nil }
        if isEditing { self.position = position; return nil }
        return position
    }

    func end(seek: (Double) -> UInt64?) {
        guard isEditing, let position else { return }
        isEditing = false
        seekRequest = seek(position)
        if seekRequest == nil { cancel() }
    }

    func supersede(with request: UInt64?) {
        if let seekRequest, seekRequest != request { cancel() }
    }

    func reconcile(_ snapshot: PlaybackSnapshot?) {
        guard let snapshot, snapshot.queue.current?.id == trackID else { cancel(); return }
        guard !isEditing, let position else { return }
        if abs(Double(snapshot.positionMs) / 1000 - position) < 0.75 || snapshot.state == .error {
            cancel()
        }
    }

    func cancel() { position = nil; isEditing = false; trackID = nil; seekRequest = nil }
}
