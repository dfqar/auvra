import AppKit
import Foundation
import Darwin

private struct SoakFailure: Error { let message: String }
@MainActor
private final class PolishSoakChecks: NSObject, NSApplicationDelegate {
    let fixture: URL
    let evidence: URL
    let seconds: Double
    var records: [[String: Any]] = []
    var commands: [Double] = []
    var jitters: [Double] = []
    var failures: [String] = []
    weak var fixtureWindow: NSWindow?
    weak var fixtureView: MetalHostView?
    var lastActivation: TimeInterval = 0
    init(fixture: URL, evidence: URL, seconds: Double) { self.fixture = fixture; self.evidence = evidence; self.seconds = seconds }
    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { @MainActor in
            do { try await run(); save(); print("M10 CoreAudio + EQ + native Metal soak passed (\(seconds) seconds)"); fflush(stdout); NSApp.terminate(nil) }
            catch { failures.append(String(describing: error)); save(); print("M10 soak failed: \(error)"); fflush(stdout); exit(1) }
        }
    }
    func save() {
        let data = try! JSONSerialization.data(withJSONObject: ["duration_requested_seconds":seconds,"samples":records,"command_latency_ms":commands,"heartbeat_jitter_ms":jitters,"failures":failures], options: [.prettyPrinted,.sortedKeys])
        try! data.write(to: evidence.appendingPathComponent("polish-soak.json"), options: .atomic)
    }
    func check(_ value: @autoclosure () -> Bool, _ message: String) throws {
        if !value() {
            recordContext(message)
            throw SoakFailure(message: message)
        }
    }
    func wait(_ label: String, seconds: Double = 10, _ condition: @MainActor () -> Bool) async throws {
        let until = ProcessInfo.processInfo.systemUptime + seconds
        while ProcessInfo.processInfo.systemUptime < until {
            if condition() { return }; keepFixtureVisible(); try await Task.sleep(for: .milliseconds(10))
        }
        recordContext("Timed out: " + label)
        throw SoakFailure(message: "Timed out: \(label)")
    }
    func recordContext(_ reason: String) {
        var context: [String: Any] = ["failure_context": reason, "app_active": NSApp.isActive,
                                      "screen_count": NSScreen.screens.count]
        if let window = fixtureWindow {
            context["window"] = ["visible": window.isVisible, "on_active_space": window.isOnActiveSpace,
                                 "occluded": !window.occlusionState.contains(.visible),
                                 "miniaturized": window.isMiniaturized, "level": window.level.rawValue,
                                 "frame": [window.frame.origin.x, window.frame.origin.y,
                                           window.frame.width, window.frame.height]]
            if let screen = window.screen {
                context["screen_frame"] = [screen.frame.origin.x, screen.frame.origin.y,
                                          screen.frame.width, screen.frame.height]
            }
        }
        if let view = fixtureView {
            context["host"] = ["attached": view.isSurfaceAttached, "animating": view.isAnimating,
                               "hidden": view.isHiddenOrHasHiddenAncestor,
                               "size": [view.bounds.width, view.bounds.height]]
            context["renderer"] = metrics(view.engine)
        }
        records.append(context)
    }
    func keepFixtureVisible() {
        guard let window = fixtureWindow, let view = fixtureView,
              window.isVisible && !view.isHiddenOrHasHiddenAncestor,
              !window.occlusionState.contains(.visible),
              ProcessInfo.processInfo.systemUptime - lastActivation > 0.75 else { return }
        lastActivation = ProcessInfo.processInfo.systemUptime
        window.orderFrontRegardless()
        let launch = NSWorkspace.OpenConfiguration(); launch.activates = true; launch.createsNewApplicationInstance = false
        NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: launch) { _, _ in }
    }
    func metrics(_ engine: EngineController) -> [String: Any] {
        guard let data = engine.exportMetrics(), let value = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return [:] }; return value
    }
    func frames(_ engine: EngineController) -> Int { metrics(engine)["presented_frames"] as? Int ?? 0 }
    func processMetrics() -> (UInt64, Int) {
        var info = mach_task_basic_info(); var count = mach_msg_type_number_t(MemoryLayout<mach_task_basic_info>.size / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &info) { pointer in pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &count) } }
        var threads: thread_act_array_t?; var threadCount: mach_msg_type_number_t = 0
        let threadResult = task_threads(mach_task_self_, &threads, &threadCount)
        if threadResult == KERN_SUCCESS, let threads {
            for index in 0..<Int(threadCount) { mach_port_deallocate(mach_task_self_, threads[index]) }
            vm_deallocate(mach_task_self_, vm_address_t(UInt(bitPattern: threads)), vm_size_t(Int(threadCount) * MemoryLayout<thread_t>.stride))
        }
        return (result == KERN_SUCCESS ? info.resident_size : 0, threadResult == KERN_SUCCESS ? Int(threadCount) : 0)
    }
    func command(_ player: PlayerStore, _ command: PlayerCommand) async throws {
        let start = ProcessInfo.processInfo.systemUptime; let id = player.send(command)
        try await wait("control acknowledgement") { !player.pendingCommands.contains(id) }
        let elapsed = (ProcessInfo.processInfo.systemUptime - start) * 1000; commands.append(elapsed)
        try check(elapsed < 1500, "Control blocked for \(elapsed) ms")
        try check(player.errorMessage == nil, "Player error: \(player.errorMessage ?? "")")
    }
    func run() async throws {
        let session = evidence.appendingPathComponent("isolated-session", isDirectory: true)
        let player = PlayerStore(libraryDirectory: session, loadSpotify: false)
        let still = StillStore()
        player.attachVisualModulation { [weak still] m in still?.setModulation(m.map { StillModulationInput(timeSeconds: $0.controls.timeSeconds, motion: $0.controls.motion, turbulence: $0.controls.turbulence, light: $0.controls.light) }) }
        still.prepare(); try await wait("GPU preparation", seconds: 30) { still.engine != nil || still.errorMessage != nil }
        try check(still.errorMessage == nil, "GPU preparation failed")
        guard var engine = still.engine else { throw SoakFailure(message: "No renderer") }
        let window = NSWindow(contentRect: NSRect(x: 20, y: 20, width: 640, height: 360), styleMask: [.titled,.closable,.resizable], backing: .buffered, defer: false)
        window.title = "Auvra M10 · CoreAudio + Metal soak"; window.isReleasedWhenClosed = false; window.level = .floating
        window.collectionBehavior = [.canJoinAllSpaces,.fullScreenAuxiliary]
        var view = MetalHostView(engine: engine); window.contentView = view; window.orderFrontRegardless(); NSApp.activate()
        fixtureWindow = window; fixtureView = view
        defer { view.shutdown(); window.close() }
        let launch = NSWorkspace.OpenConfiguration(); launch.activates = true; launch.createsNewApplicationInstance = false
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: launch) { _, error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume() }
            }
        }
        try await wait("native Present", seconds: 120) { frames(engine) > 3 }
        // Warm every scene before measuring steady memory and latency.
        for scene in still.scenes { let before = frames(engine); still.select(scene.id); try await wait("warm \(scene.id)") { frames(engine) > before + 5 } }
        still.select("frost")
        try await wait("storage") { player.snapshot?.session.ready == true && player.snapshot?.sound?.ready == true }
        player.setVolume(0.25); player.send(.setRepeat(.one)); player.openFile(fixture)
        try await wait("real audio") { player.audioAnalysis != nil && player.snapshot?.outputStatistics != nil }
        let start = ProcessInfo.processInfo.systemUptime
        var cycle = 0; var maximumUnderruns: UInt64 = 0; var maximumDrops: UInt64 = 0
        var baselineMemory: UInt64 = 0; var baselineThreads = 0; var peakMemory: UInt64 = 0; var peakThreads = 0
        let activity = ProcessInfo.processInfo.beginActivity(options: [.userInitiated,.idleDisplaySleepDisabled], reason: "Auvra explicit CoreAudio and Metal soak")
        defer { ProcessInfo.processInfo.endActivity(activity) }
        var visibleSeconds = 0.0
        while visibleSeconds < seconds {
            let elapsed = ProcessInfo.processInfo.systemUptime - start
            try check(elapsed < max(120, seconds * 2), "Soak window remained occluded too long")
            window.orderFrontRegardless()
            var sound = player.soundSettings
            sound.equalizer.enabled = cycle % 3 != 0; sound.equalizer.preampDb = -4
            sound.equalizer.bandsDb = [cycle % 2 == 0 ? 3 : -3, 1, 0, 2, -2]
            try await command(player, .setSound(sound))
            if cycle % 3 == 1 { try await command(player, .pause); try await command(player, .seek(positionMs: 8500)); try await command(player, .play) }
            if cycle % 6 == 2 { still.select(still.scenes[(cycle / 6) % still.scenes.count].id) }
            if cycle > 0 && cycle % 12 == 0 {
                view.isHidden = true; try await wait("hidden host") { !view.isAnimating }
                let hidden = frames(engine); try await Task.sleep(for: .milliseconds(200)); try check(frames(engine) == hidden, "Hidden host rendered")
                view.isHidden = false; try await wait("visible host") { frames(engine) > hidden }
            }
            if cycle > 0 && cycle % 30 == 0 {
                view.shutdown(); still.retry()
                try await wait("renderer Retry") { still.engine != nil && still.engine !== engine }
                engine = still.engine!; view = MetalHostView(engine: engine); window.contentView = view; window.orderFrontRegardless()
                fixtureView = view
                try await wait("Present after Retry") { frames(engine) > 3 }
            }
            let intervalFrames = frames(engine)
            let visibleBefore = visibleSeconds
            let until = ProcessInfo.processInfo.systemUptime + min(10, max(0, seconds - visibleSeconds))
            while ProcessInfo.processInfo.systemUptime < until {
                let tick = ProcessInfo.processInfo.systemUptime
                try await Task.sleep(for: .milliseconds(50))
                let delta = ProcessInfo.processInfo.systemUptime - tick
                keepFixtureVisible()
                jitters.append(max(0, delta * 1000 - 50))
                if view.isAnimating && window.occlusionState.contains(.visible) { visibleSeconds += delta }
            }
            try check(player.errorMessage == nil && still.errorMessage == nil, "Playback or renderer failed")
            let presented = frames(engine)
            records.append(["visible_seconds":visibleSeconds,"interval_visible_seconds":visibleSeconds-visibleBefore,"interval_frames":presented-intervalFrames,"animating":view.isAnimating,"visible":window.isVisible,"occluded":!window.occlusionState.contains(.visible),"app_active":NSApp.isActive])
            try check(visibleSeconds - visibleBefore < 5 || presented > intervalFrames + 30, "Visible renderer stalled")
            try check(player.snapshot?.state == .playing, "Repeat playback stopped")
            let stats = player.snapshot?.outputStatistics
            maximumUnderruns = max(maximumUnderruns, stats?.underrunFrames ?? 0); maximumDrops = max(maximumDrops, stats?.analysisDroppedFrames ?? 0)
            let (memory, threads) = processMetrics(); peakMemory = max(peakMemory, memory); peakThreads = max(peakThreads, threads)
            if baselineMemory == 0 && elapsed >= 60 { baselineMemory = memory; baselineThreads = threads }
            records.append(["seconds":ProcessInfo.processInfo.systemUptime - start,"resident_bytes":memory,"threads":threads,"state":player.snapshot?.state.rawValue ?? "none","audio_callbacks":stats?.callbacks ?? 0,"audio_media_frames":stats?.mediaFrames ?? 0,"underrun_frames":stats?.underrunFrames ?? 0,"analysis_drops":stats?.analysisDroppedFrames ?? 0,"renderer":metrics(engine)])
            save(); print("Soak \(Int(ProcessInfo.processInfo.systemUptime - start))/\(Int(seconds)) s · RSS \(memory / 1048576) MiB · threads \(threads) · underruns \(maximumUnderruns) · drops \(maximumDrops)"); fflush(stdout)
            cycle += 1
        }
        let sorted = commands.sorted(); let p99 = sorted[min(sorted.count - 1, Int(Double(sorted.count) * 0.99))]
        try check(p99 < 500, "Control p99 \(p99) ms exceeds 500 ms")
        try check(maximumUnderruns == 0 && maximumDrops == 0, "Output underruns or analysis overflow during soak")
        if seconds >= 120 {
            let final = processMetrics()
            try check(final.0 <= baselineMemory + 96 * 1048576, "RSS grew >96 MiB after warmup")
            try check(final.1 <= baselineThreads + 8 && peakThreads <= baselineThreads + 12, "Worker/thread count grew")
        }
        records.append(["summary":true,"actual_seconds":ProcessInfo.processInfo.systemUptime-start,"visible_seconds":visibleSeconds,"cycles":cycle,"control_p99_ms":p99,"peak_resident_bytes":peakMemory,"baseline_resident_bytes":baselineMemory,"baseline_threads":baselineThreads,"peak_threads":peakThreads,"underruns":maximumUnderruns,"analysis_drops":maximumDrops])
        await player.prepareToQuit(); await still.shutdown()
    }
}
@main
struct PolishSoak {
    @MainActor static func main() {
        let app = NSApplication.shared; app.setActivationPolicy(.regular)
        let root = Bundle.main.bundleURL.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let fixture = CommandLine.arguments.count > 1 ? URL(fileURLWithPath: CommandLine.arguments[1]) : root.appendingPathComponent("build/modulation-fixtures/Auvra Modulation Check.wav")
        let evidence = CommandLine.arguments.count > 2 ? URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true) : root.appendingPathComponent("build/m10-verification/soak-native-launch", isDirectory: true)
        try! FileManager.default.createDirectory(at: evidence, withIntermediateDirectories: true)
        let duration = CommandLine.arguments.count > 3 ? Double(CommandLine.arguments[3]) ?? 1800 : 1800
        let delegate = PolishSoakChecks(fixture: fixture, evidence: evidence, seconds: duration)
        app.delegate = delegate; app.run(); withExtendedLifetime(delegate) {}
    }
}
