import AppKit
import CoreAudio
import Foundation
import OSLog

// Native service boundaries. Providers and platform frameworks never enter Views.
protocol CredentialStore: Sendable {
    func read(account: String) async throws -> Data?
    func write(_ value: Data, account: String) async throws
    func remove(account: String) async throws
}
@MainActor
protocol SystemMediaSession: AnyObject {
    // Publish only for a capable source or an acquired native output lease.
    func publish(_ state: PlaybackSnapshot)
    func clear()
}
@MainActor
protocol ArtworkLoader {
    func image(for reference: String) async throws -> NSImage
}
@MainActor
protocol WindowService {
    func showMiniPlayer()
    func showMainPlayer()
}

enum SpatialOutputPreflight {
    static func start() {
        guard #available(macOS 14.2, *) else { return }
        _ = initializeDefaultOutput
    }

    private static let initializeDefaultOutput: Void = {
        DispatchQueue.global(qos: .utility).async {
            let trace = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
            let log = Logger(subsystem: "app.auvra.player", category: "PlaybackTiming")
            if trace { log.info("spatial_route_preflight_start") }
            var address = AudioObjectPropertyAddress(
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain
            )
            var output = AudioDeviceID(kAudioObjectUnknown)
            var size = UInt32(MemoryLayout<AudioDeviceID>.size)
            // Initialize HAL off the playback path. Each actual relay verifies the live route again.
            let status = AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &output)
            if trace { log.info("spatial_route_preflight_done available=\(status == noErr && output != kAudioObjectUnknown, privacy: .public)") }
        }
    }()
}
