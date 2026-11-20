import AppKit
import SwiftUI
import Foundation
import ImageIO

private struct PolishFailure: Error { let message: String }
private actor ArtworkLoadProbe {
    var calls: [String: Int] = [:]
    var pending: [String: [CheckedContinuation<DecodedArtwork?, Never>]] = [:]
    func load(_ reference: String) async -> DecodedArtwork? {
        calls[reference, default: 0] += 1
        return await withCheckedContinuation { pending[reference, default: []].append($0) }
    }
    func count(_ reference: String) -> Int { calls[reference, default: 0] }
    func pendingCount() -> Int { pending.values.reduce(0) { $0 + $1.count } }
    func resolve(_ reference: String, value: DecodedArtwork) {
        guard pending[reference]?.isEmpty == false else { return }
        pending[reference]!.removeFirst().resume(returning: value)
    }
}
@MainActor
private final class PolishChecks: NSObject, NSApplicationDelegate {
    let fixture: URL; let evidence: URL
    let stateDirectory: URL
    var checks: [String] = []; var failures: [String] = []
    init(fixture: URL, evidence: URL) {
        self.fixture = fixture; self.evidence = evidence
        stateDirectory = evidence.appendingPathComponent("session-" + UUID().uuidString, isDirectory: true)
    }
    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { @MainActor in
            do { try await run(); save(); print("M10 native polish passed: sound/SQLite/post-DSP analysis, paused restore, core restart, local decode Retry and shared mini/settings"); NSApp.terminate(nil) }
            catch { failures.append(String(describing: error)); save(); print("M10 native polish failed: \(error)"); exit(1) }
        }
    }
    func save() {
        let data = try! JSONSerialization.data(withJSONObject: ["checks":checks,"failures":failures], options: [.prettyPrinted,.sortedKeys])
        try! data.write(to: evidence.appendingPathComponent("polish-native.json"), options: .atomic)
    }
    func check(_ value: @autoclosure () -> Bool, _ message: String) throws { if !value() { throw PolishFailure(message: message) } }
    func wait(_ label: String, allowingError: Bool = false, player: PlayerStore, _ condition: @MainActor () -> Bool) async throws {
        for _ in 0..<600 {
            if !allowingError, let error = player.errorMessage { throw PolishFailure(message: "\(label): \(error)") }
            if condition() { return }; try await Task.sleep(for: .milliseconds(10))
        }
        let snapshot = player.snapshot
        let analysis = player.audioAnalysis
        // Numeric diagnostics only: never include file paths, account metadata or credentials.
        let output = snapshot?.outputStatistics.map {
            "callbacks=\($0.callbacks),mediaFrames=\($0.mediaFrames),nonzeroFrames=\($0.nonzeroFrames),underrunFrames=\($0.underrunFrames),analysisDroppedFrames=\($0.analysisDroppedFrames)"
        } ?? "none"
        let features = analysis.map {
            "rate=\($0.sampleRate),windowEnd=\($0.windowEndFrame),positionMs=\($0.features.playbackPositionMs),rms=\($0.features.rms)"
        } ?? "none"
        throw PolishFailure(message: "Timed out: \(label), state=\(snapshot?.state.rawValue ?? "none"), positionMs=\(snapshot?.positionMs ?? 0), volume=\(snapshot?.volume ?? -1), output={\(output)}, analysis={\(features)}, sound=\(String(describing: snapshot?.sound))")
    }
    func note(_ value: String) { checks.append(value); print("Passed: \(value)"); fflush(stdout); save() }
    func artworkChecks() async throws {
        for host in ["i.scdn.co","mosaic.scdn.co","image-cdn-fa.spotifycdn.com","image-cdn-ak.spotifycdn.com"] {
            try check(RemoteArtworkURL.validated("https://\(host)/image/original-cover-fixture") != nil, "A Spotify artwork CDN was rejected")
        }
        for url in ["http://image-cdn-fa.spotifycdn.com/image/x","https://image-cdn-fa.spotifycdn.com.evil.invalid/image/x","https://image-cdn-ak.spotifycdn.com@evil.invalid/image/x","https://user@image-cdn-fa.spotifycdn.com/image/x","https://image-cdn-fa.spotifycdn.com:444/image/x","https://image-cdn-fa.spotifycdn.com/other/x","https://evil.invalid/image/x"] {
            try check(RemoteArtworkURL.validated(url) == nil, "Artwork URL escaped the exact HTTPS CDN policy")
        }
        note("artwork URLs: track/mosaic and observed custom-cover CDNs accepted; lookalikes, credentials, ports and foreign paths rejected")
        let a = NSImage(size: NSSize(width: 16,height: 16)); let c = NSImage(size: NSSize(width: 16,height: 16))
        var delayed: CheckedContinuation<NSImage?, Never>?
        let presentation = ArtworkPresentation { reference in
            if reference == "/slow" { return await withCheckedContinuation { delayed = $0 } }
            return reference == "/a" ? a : c
        }
        await presentation.load("/a", preference: true, reduceMotion: false)
        try check(presentation.image === a, "Initial artwork missing")
        let old = Task { @MainActor in await presentation.load("/slow", preference: true, reduceMotion: false) }
        for _ in 0..<100 { if delayed != nil { break }; try await Task.sleep(for: .milliseconds(1)) }
        try await Task.sleep(for: .milliseconds(150))
        try check(presentation.image == nil, "Old cover remained beyond 120 ms")
        await presentation.load("/c", preference: true, reduceMotion: false)
        delayed?.resume(returning: a); delayed = nil; await old.value
        try check(presentation.image === c && presentation.reference == "/c", "Delayed artwork replaced a newer track")
        let cancelled = Task { @MainActor in await presentation.load("/slow", preference: true, reduceMotion: false) }
        for _ in 0..<100 { if delayed != nil { break }; try await Task.sleep(for: .milliseconds(1)) }
        cancelled.cancel(); presentation.cancel(); delayed?.resume(returning: a); await cancelled.value
        try check(presentation.image == nil, "Cancelled artwork request returned after disappearance")
        try check(!ArtworkPresentation.allowsTransition(from: "/a", to: "https://i.scdn.co/image/x", preference: true, reduceMotion: false), "Remote artwork animated")
        try check(!ArtworkPresentation.allowsTransition(from: "/a", to: "/c", preference: true, reduceMotion: true), "Reduce Motion ignored")
        try check(!ArtworkPresentation.allowsTransition(from: "/a", to: "/c", preference: false, reduceMotion: false), "Artwork preference ignored")
        try check(ArtworkPresentation.allowsTransition(from: "/a", to: "/c", preference: true, reduceMotion: false), "Local transition unavailable")
        note("artwork: bounded stale cover, out-of-order completion, cancellation, static remote and Reduce Motion policy")
        let probe = ArtworkLoadProbe()
        let loader = LocalArtworkLoader(operation: { await probe.load($0) })
        let remote = "https://i.scdn.co/image/original-test-reference"
        var received: [Int: NSImage] = [:]
        let first = Task { @MainActor in if let image = await loader.image(for: remote) { received[0] = image } }
        for _ in 0..<100 { if await probe.count(remote) == 1 { break }; try await Task.sleep(for: .milliseconds(1)) }
        loader.discardRemote()
        let second = Task { @MainActor in if let image = await loader.image(for: remote) { received[1] = image } }
        for _ in 0..<100 { if await probe.count(remote) == 2 { break }; try await Task.sleep(for: .milliseconds(1)) }
        guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil,pixelsWide: 16,pixelsHigh: 16,bitsPerSample: 8,samplesPerPixel: 4,hasAlpha: true,isPlanar: false,colorSpaceName: .deviceRGB,bytesPerRow: 64,bitsPerPixel: 32), let cg = bitmap.cgImage else { throw PolishFailure(message: "Own artwork fixture failed") }
        let decoded = DecodedArtwork(image: cg)
        await probe.resolve(remote, value: decoded); await first.value
        let third = Task { @MainActor in if let image = await loader.image(for: remote) { received[2] = image } }
        try await Task.sleep(for: .milliseconds(25))
        let count = await probe.count(remote)
        try check(count == 2, "Cancelled completion removed the newer coalesced artwork request")
        await probe.resolve(remote, value: decoded); await second.value; await third.value
        try check(received[0] == nil && received[1] != nil && received[2] != nil, "Artwork cancellation/coalescing lost a current consumer")
        note("artwork cache: disconnect/reconnect of the same reference cannot remove the new in-flight request")
        var bounded: [Task<Void, Never>] = []
        for index in 0..<64 {
            let path = "https://i.scdn.co/image/bounded-\(index)"
            bounded.append(Task { @MainActor in _ = await loader.image(for: path) })
        }
        for _ in 0..<100 { if await probe.pendingCount() == 64 { break }; try await Task.sleep(for: .milliseconds(1)) }
        let waiting = await probe.pendingCount()
        try check(waiting == 64, "Bounded artwork probe did not start all 64 operations")
        loader.discardRemote()
        let overflow = await loader.image(for: "https://i.scdn.co/image/overflow")
        let overflowCalls = await probe.count("https://i.scdn.co/image/overflow")
        try check(overflow == nil && overflowCalls == 0, "Cancelled artwork operations released their capacity before finishing")
        for index in 0..<64 { await probe.resolve("https://i.scdn.co/image/bounded-\(index)", value: decoded) }
        for task in bounded { await task.value }
        let afterDrain = Task { @MainActor in _ = await loader.image(for: "https://i.scdn.co/image/after-drain") }
        for _ in 0..<100 { if await probe.count("https://i.scdn.co/image/after-drain") == 1 { break }; try await Task.sleep(for: .milliseconds(1)) }
        let admitted = await probe.count("https://i.scdn.co/image/after-drain")
        try check(admitted == 1, "Artwork capacity did not recover after cancelled IO drained")
        await probe.resolve("https://i.scdn.co/image/after-drain", value: decoded); await afterDrain.value
        note("artwork IO stays bounded across mass cancellation and admits new work after draining")
        let paletteFile = evidence.appendingPathComponent("original-red-cover.png")
        guard let context = CGContext(data: nil, width: 32, height: 32, bitsPerComponent: 8, bytesPerRow: 128, space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue),
              let destination = CGImageDestinationCreateWithURL(paletteFile as CFURL, "public.png" as CFString, 1, nil) else { throw PolishFailure(message: "Own palette fixture failed") }
        context.setFillColor(red: 0.9, green: 0.05, blue: 0.1, alpha: 1); context.fill(CGRect(x: 0, y: 0, width: 32, height: 32))
        guard let redImage = context.makeImage() else { throw PolishFailure(message: "Own palette image failed") }
        CGImageDestinationAddImage(destination, redImage, nil)
        try check(CGImageDestinationFinalize(destination), "Palette fixture could not be saved")
        let paletteLoader = LocalArtworkLoader()
        let palette = await paletteLoader.palette(for: paletteFile.path)
        try check(palette?.colors.count == 3 && palette!.colors.allSatisfy { $0[0] > 0.8 && $0[1] < 0.15 && $0[2] < 0.2 }, "Cover palette lost its actual hue")
        let cachedPalette = await paletteLoader.palette(for: paletteFile.path)
        try check(cachedPalette == palette, "Coalesced artwork cache discarded its palette")
        let missingPalette = await paletteLoader.palette(for: evidence.appendingPathComponent("missing-cover.png").path)
        try check(missingPalette == nil, "Missing cover inherited a stale palette")
        note("ambient cover palette: bounded sRGB decode, dominant hue, shared cache and missing-cover release")
    }
    func run() async throws {
        try await artworkChecks()
        let earlyDirectory = stateDirectory.appendingPathComponent("early-session", isDirectory: true)
        let early = PlayerStore(libraryDirectory: earlyDirectory)
        early.updateSound { $0.equalizer.preampDb = -5 }
        await early.prepareToQuit()
        let reopenedEarly = PlayerStore(libraryDirectory: earlyDirectory)
        try await wait("early edit persisted through quit", player: reopenedEarly) { reopenedEarly.snapshot?.sound?.ready == true }
        try check(reopenedEarly.soundSettings.equalizer.preampDb == -5 && !reopenedEarly.isPlaying, "Early quit lost accepted sound settings")
        await reopenedEarly.prepareToQuit()
        note("sound edit immediately after startup survives quit before initial storage projection")
        let directory = stateDirectory.appendingPathComponent("isolated-session", isDirectory: true)
        let player = PlayerStore(libraryDirectory: directory)
        try await wait("storage startup", player: player) { player.snapshot?.sound?.ready == true && player.snapshot?.session.ready == true }
        try check(!player.soundSettings.equalizer.enabled, "Fresh EQ was enabled")
        player.setVolume(0.25); player.openFile(fixture)
        try await wait("steady 1 kHz baseline", player: player) {
            guard let f = player.audioAnalysis else { return false }
            return f.features.playbackPositionMs >= 180 && abs(f.features.rms - 0.04 * 0.25 / sqrt(2)) < 0.00005
        }
        let baseline = player.audioAnalysis!.features.rms
        player.updateSound { $0.equalizer.enabled = true; $0.equalizer.bandsDb[2] = 6
            $0.presets = [EqualizerPreset(name: "Native voice", equalizer: $0.equalizer)] }
        try await wait("real post-DSP +6 dB", player: player) { abs((player.audioAnalysis?.features.rms ?? 0) / baseline - 1.99526) < 0.03 }
        let expected = player.soundSettings
        try await wait("sound saved", player: player) { player.snapshot?.sound?.settings == expected && player.snapshot?.sound?.saving == false && player.pendingCommands.isEmpty }
        note("real Swift command → worker EQ → CoreAudio → post-DSP analysis; preset saved")
        player.send(.pause)
        try await wait("pause", player: player) { player.snapshot?.state == .paused }
        player.seek(seconds: 1.5)
        try await wait("paused position", player: player) { player.snapshot?.positionMs == 1500 && player.pendingCommands.isEmpty }
        await player.prepareToQuit()
        let restored = PlayerStore(libraryDirectory: directory)
        try await wait("paused restore with sound", player: restored) { restored.snapshot?.sound?.ready == true && restored.snapshot?.session.restored == true }
        try check(restored.snapshot?.state == .paused && restored.snapshot?.positionMs == 1500, "Restore changed playback intent or position")
        try check(restored.soundSettings == expected && restored.volume == 0.25, "Sound, preset or volume changed during restore")
        try check(restored.audioAnalysis == nil && restored.visualModulation == nil && restored.snapshot?.playbackSessionActive == false, "Unplayed restore claimed an active session")
        note("queue/position/volume/EQ/preset restore together on pause; no analysis or OS ownership")
        restored.send(.play)
        try await wait("play before core restart", player: restored) { restored.audioAnalysis != nil }
        let before = restored.snapshot!.positionMs
        await restored.restartPlayer()
        try await wait("core restarted", player: restored) { restored.snapshot?.session.restored == true && restored.snapshot?.sound?.ready == true }
        try check(restored.snapshot?.state == .paused && restored.snapshot!.positionMs >= before, "Restart lost position or auto-played")
        try check(restored.soundSettings == expected && restored.audioAnalysis == nil, "Restart lost EQ or retained old analysis")
        note("core restart invalidates old bridge events and restores paused with EQ")
        let recoverable = evidence.appendingPathComponent("Recoverable.wav")
        try Data("invalid original test WAV".utf8).write(to: recoverable)
        restored.openFile(recoverable)
        try await wait("decode failure", allowingError: true, player: restored) { restored.snapshot?.state == .error && restored.canRetryPlayback }
        let id = restored.current?.id
        try Data(contentsOf: fixture).write(to: recoverable)
        restored.retryPlayback()
        try await wait("file Retry", player: restored) { restored.audioAnalysis != nil && restored.isPlaying }
        try check(restored.current?.id == id, "Retry replaced queue identity")
        note("repaired local file uses explicit Retry and keeps track identity")
        restored.send(.pause)
        try await wait("pause for native windows", player: restored) { restored.snapshot?.state == .paused }
        let still = StillStore()
        let window = NSWindow(contentRect: NSRect(x: 100,y: 100,width: 540,height: 630), styleMask: [.titled,.closable,.miniaturizable], backing: .buffered, defer: false)
        window.title = "Auvra M10 settings check"; window.isReleasedWhenClosed = false
        window.contentView = NSHostingView(rootView: SettingsView(player: restored, still: still))
        window.makeKeyAndOrderFront(nil)
        let navigation = AppNavigation(); let windows = MacWindowService()
        windows.connect(player: restored, navigation: navigation, openMain: { window.makeKeyAndOrderFront(nil) }); windows.registerMain(window)
        windows.showMiniPlayer(); try check(windows.miniVisible, "Mini did not open")
        restored.setVolume(0.31)
        try await wait("shared volume", player: restored) { abs(restored.volume - 0.31) < 0.001 }
        windows.showMainPlayer(); try check(!windows.miniVisible, "Main did not reuse mini session")
        windows.showMiniPlayer(); windows.toggleMiniPlayer()
        try check(!windows.miniVisible && restored.snapshot?.state == .paused, "Mini lifecycle changed transport")
        note("native settings; reusable mini shares paused transport and volume")
        window.miniaturize(nil)
        try await wait("main window minimized in the Dock", player: restored) { window.isMiniaturized }
        windows.showMainPlayer()
        try await wait("minimized main window restored", player: restored) { !window.isMiniaturized && window.isVisible }
        try check(restored.snapshot?.state == .paused, "Reopening the minimized main window changed transport")
        note("reopening the main window restores it from the Dock and preserves pause")
        windows.shutdown(); window.close(); await restored.prepareToQuit(); await still.shutdown()
    }
}
@main
struct PolishSmoke {
    @MainActor static func main() {
        let app = NSApplication.shared; app.setActivationPolicy(.regular)
        let delegate = PolishChecks(fixture: URL(fileURLWithPath: CommandLine.arguments[1]), evidence: URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true))
        app.delegate = delegate; app.run(); withExtendedLifetime(delegate) {}
    }
}
