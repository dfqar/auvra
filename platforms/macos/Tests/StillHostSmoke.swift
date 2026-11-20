import AppKit
import Foundation

private struct StillSmokeFailure: Error { let message: String }
// Only this test window can inject a visibility signal. It never substitutes
// for the OS coverage check: an unavailable OS signal makes the run inconclusive.
@MainActor
private final class FixtureWindow: NSWindow {
    var forceOccluded = false
    override var occlusionState: NSWindow.OcclusionState { forceOccluded ? [] : super.occlusionState }
}
private final class FactoryCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    func next() -> Int { lock.lock(); defer { lock.unlock() }; count += 1; return count }
}
@MainActor
private final class FullscreenState: NSObject, NSWindowDelegate {
    var entered = false
    var exited = false
    func windowDidEnterFullScreen(_ notification: Notification) { entered = true }
    func windowDidExitFullScreen(_ notification: Notification) { exited = true }
}
@MainActor
private final class StillHostChecks: NSObject, NSApplicationDelegate {
    var records: [[String: Any]] = []
    var failures: [String] = []
    var unverified: [String] = []
    var complete = false
    var renderErrors: [String] = []
    private var lastActivation: TimeInterval = 0
    weak var currentView: MetalHostView?
    weak var currentEngine: EngineController?
    let directory: URL
    init(directory: URL) { self.directory = directory }
    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { @MainActor in
            do {
                try await run(); complete = true; save()
                if !unverified.isEmpty { print("M8 Still host inconclusive: \(unverified.joined(separator: "; ")); remaining lifecycle checks passed"); exit(2) }
                print("M8 Still host passed: real Metal surface/Present, six scenes, Retina/resize/zero/fullscreen, inactive first paint, visibility/minimize/sleep signals, stale host isolation, retry/cancel and cleanup"); NSApp.terminate(nil)
            }
            catch { failures.append(String(describing: error)); save(); print("M8 Still host failed: \(error)"); exit(1) }
        }
    }
    func save() {
        let data = try! JSONSerialization.data(withJSONObject: ["checks": records, "failures": failures, "unverified": unverified, "complete": complete], options: [.prettyPrinted, .sortedKeys])
        try! data.write(to: directory.appendingPathComponent("native-smoke.json"), options: .atomic)
    }
    func check(_ condition: @autoclosure () -> Bool, _ message: String) throws { if !condition() { throw StillSmokeFailure(message: message) } }
    func wait(_ label: String, attempts: Int = 200, foreground: Bool = false, _ condition: @MainActor () -> Bool) async throws {
        for _ in 0..<attempts {
            if foreground && !NSApp.isActive && ProcessInfo.processInfo.systemUptime - lastActivation > 0.75 {
                lastActivation = ProcessInfo.processInfo.systemUptime
                let launch = NSWorkspace.OpenConfiguration(); launch.activates = true; launch.createsNewApplicationInstance = false
                NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: launch) { _, _ in }
            }
            if condition() { return }; try await Task.sleep(for: .milliseconds(25))
        }
        if let engine = currentEngine, let view = currentView { record("timeout: " + label, engine, view) }
        throw StillSmokeFailure(message: "Timed out: \(label), active=\(NSApp.isActive), visible=\(currentView?.window?.isVisible == true), occlusion=\(currentView?.window?.occlusionState.contains(.visible) == true), animating=\(currentView?.isAnimating == true)")
    }
    func metrics(_ engine: EngineController) -> [String: Any] {
        guard let data = engine.exportMetrics(), let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return [:] }
        return object
    }
    func frames(_ engine: EngineController) -> Int { metrics(engine)["presented_frames"] as? Int ?? 0 }
    func sceneFrames(_ engine: EngineController, _ scene: String) -> Int {
        let scenes = metrics(engine)["scenes"] as? [String: [String: Any]]
        return scenes?[scene]?["presented_frames"] as? Int ?? 0
    }
    func record(_ phase: String, _ engine: EngineController, _ view: MetalHostView) {
        records.append(["phase": phase, "active": NSApp.isActive, "animating": view.isAnimating,
                        "attached": view.isSurfaceAttached, "bounds_points": [view.bounds.width, view.bounds.height],
                        "scale": view.window?.backingScaleFactor ?? 0, "metrics": metrics(engine)])
        print("Passed: \(phase)")
    }
    func stationary(_ label: String, _ engine: EngineController, _ view: MetalHostView) async throws {
        try await wait(label + " suspend") { !view.isAnimating }
        try await Task.sleep(for: .milliseconds(100)) // Drain one already-admitted display callback.
        let before = frames(engine)
        try await Task.sleep(for: .milliseconds(250))
        try check(frames(engine) == before, label + " continued presenting")
        record(label, engine, view)
    }
    func makeWindow(_ title: String, _ size: CGSize) -> FixtureWindow {
        let window = FixtureWindow(contentRect: NSRect(origin: CGPoint(x: 120, y: 100), size: size), styleMask: [.titled, .closable, .resizable, .miniaturizable], backing: .buffered, defer: false)
        window.title = title; window.isReleasedWhenClosed = false; return window
    }
    func close(_ engine: EngineController) async {
        await withCheckedContinuation { continuation in engine.close { continuation.resume() } }
    }
    func run() async throws {
        let engine = EngineController(quality: 0)
        try check(engine.initializationError == nil, "Metal unavailable: \(engine.initializationError ?? "")")
        engine.onError = { [weak self] message in Task { @MainActor in self?.renderErrors.append(message) } }
        try check(engine.selectScene("frost"), "Frost selection failed")
        let window = makeWindow("Auvra Still integration check", CGSize(width: 800, height: 500))
        let initialColor = CGColor(srgbRed: 0.24, green: 0.11, blue: 0.18, alpha: 1)
        let view = MetalHostView(engine: engine, placeholderColor: initialColor)
        try check(view.layer?.backgroundColor == initialColor && view.layer?.isOpaque == true,
                  "The initial album tone was absent before the Metal surface mounted")
        currentView = view; currentEngine = engine; window.contentView = view
        let nextColor = CGColor(srgbRed: 0.12, green: 0.29, blue: 0.26, alpha: 1)
        view.setPlaceholderColor(nextColor)
        try check(view.layer?.backgroundColor == nextColor && view.layer?.isOpaque == true,
                  "The changed album tone did not reach the opaque Metal layer")
        record("opaque album-tone placeholder before first paint", engine, view)
        window.orderFrontRegardless()
        defer { view.shutdown(); window.close() }
        try await wait("inactive first paint") { frames(engine) > 0 }
        if !NSApp.isActive { try await stationary("inactive first paint then freeze", engine, view) }
        if !NSApp.isActive {
            let before = frames(engine)
            engine.updateAmbient(colors: [(0.2,0.2,0.2),(0.5,0.5,0.5),(0.8,0.8,0.8)])
            view.setContentRevision(1)
            try await wait("inactive palette repaint") { frames(engine) > before }
            try await stationary("inactive palette repaint then freeze", engine, view)
        }
        // The CLI-created test process reopens only its own bundle through LaunchServices.
        // This gives the harness the same normal activation path as opening the app.
        let launch = NSWorkspace.OpenConfiguration(); launch.activates = true; launch.createsNewApplicationInstance = false
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: launch) { _, error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume() }
            }
        }
        NSApp.activate(); window.makeKeyAndOrderFront(nil)
        print("Waiting for native foreground activation"); fflush(stdout)
        try await wait("display link / real Present", attempts: 1200, foreground: true) { NSApp.isActive && view.isAnimating && frames(engine) > 10 }
        record("active native surface", engine, view)
        let expected = [Int((view.bounds.width * window.backingScaleFactor).rounded()), Int((view.bounds.height * window.backingScaleFactor).rounded())]
        try check(metrics(engine)["viewport_pixels"] as? [Int] == expected, "Retina viewport does not match points × backing scale")
        let catalog = engine.sceneCatalog().scenes.map(\.id).sorted()
        try check(catalog == ["frost"], "Auvra must expose exactly its Frost scene")
        for scene in catalog {
            NSApp.activate(); window.makeKeyAndOrderFront(nil)
            let before = sceneFrames(engine, scene)
            try check(engine.selectScene(scene), "Cannot select \(scene)")
            try await wait("real scene Present: " + scene, foreground: true) { sceneFrames(engine, scene) >= before + 12 }
            try check(engine.sceneCatalog().selected == scene, "Scene selection mismatch")
            record("scene " + scene, engine, view)
        }
        try check(!engine.selectScene("unsupported-scene"), "Invalid scene unexpectedly accepted")
        for size in [CGSize(width: 620, height: 380), CGSize(width: 1000, height: 640), CGSize(width: 800, height: 500)] {
            window.setContentSize(size)
            try await wait("resize") {
                let scale = window.backingScaleFactor
                return metrics(engine)["viewport_pixels"] as? [Int] == [Int((size.width*scale).rounded()), Int((size.height*scale).rounded())]
            }
            record("resize \(Int(size.width))x\(Int(size.height))", engine, view)
        }
        let size = view.frame.size
        view.setFrameSize(.zero); view.needsLayout = true; view.layoutSubtreeIfNeeded()
        try await wait("zero viewport") { metrics(engine)["viewport_pixels"] as? [Int] == [0,0] }
        try await stationary("zero size", engine, view)
        view.setFrameSize(size); view.needsLayout = true; view.layoutSubtreeIfNeeded()
        let beforeRestore = frames(engine)
        try await wait("zero restore", foreground: true) { frames(engine) > beforeRestore + 5 }
        record("zero size restored", engine, view)
        view.isHidden = true; try await stationary("hidden view", engine, view)
        view.isHidden = false
        let unhidden = frames(engine); try await wait("unhide", foreground: true) { frames(engine) > unhidden + 5 }
        // A separate owned fixture process provides an opaque occluder. This
        // avoids treating same-process window composition as an OS occlusion signal.
        // Cover the entire display, including any composition outside NSWindow.frame.
        // A fixture ready report makes a missing/failed child an actual test failure.
        let frame = (window.screen?.frame ?? window.frame).insetBy(dx: -64, dy: -64)
        let coverReady = directory.appendingPathComponent("cover-ready.json")
        try? FileManager.default.removeItem(at: coverReady)
        let cover = Process(); cover.executableURL = Bundle.main.executableURL
        cover.arguments = ["--cover", String(describing: frame.minX), String(describing: frame.minY), String(describing: frame.width), String(describing: frame.height), coverReady.path]
        try cover.run()
        defer { if cover.isRunning { cover.terminate() } }
        try await wait("opaque fixture ready") { FileManager.default.fileExists(atPath: coverReady.path) }
        let coverState = try JSONSerialization.jsonObject(with: Data(contentsOf: coverReady)) as? [String: Any]
        try check(coverState?["visible"] as? Bool == true && coverState?["opaque"] as? Bool == true, "Cover fixture was not visible and opaque")
        guard let coverBounds = coverState?["frame"] as? [Double], coverBounds.count == 4 else { throw StillSmokeFailure(message: "Cover fixture omitted its actual frame") }
        try check(NSRect(x: coverBounds[0], y: coverBounds[1], width: coverBounds[2], height: coverBounds[3]).contains(window.frame), "Cover fixture did not fully contain the target window")
        for _ in 0..<200 {
            if !window.occlusionState.contains(.visible) { break }
            try await Task.sleep(for: .milliseconds(25))
        }
        if window.occlusionState.contains(.visible) {
            unverified.append("OS did not report full-window occlusion; physical coverage suspension is unverified")
            records.append(["phase":"OS full-window occlusion unavailable", "raw_occlusion":window.occlusionState.rawValue, "verified":false])
            print("Unverified: OS full-window occlusion; checking the handler separately with an injected signal")
            window.forceOccluded = true
            NotificationCenter.default.post(name: NSWindow.didChangeOcclusionStateNotification, object: window)
            try await stationary("injected occlusion signal", engine, view)
            window.forceOccluded = false
            NotificationCenter.default.post(name: NSWindow.didChangeOcclusionStateNotification, object: window)
        } else { try await stationary("OS occluded window", engine, view) }
        cover.terminate()
        try await wait("occluder exited") { !cover.isRunning }
        window.makeKeyAndOrderFront(nil); NSApp.activate()
        let uncovered = frames(engine); try await wait("uncover", foreground: true) { frames(engine) > uncovered + 5 }
        window.miniaturize(nil); try await wait("minimized") { window.isMiniaturized }
        try await stationary("minimized window", engine, view)
        window.deminiaturize(nil); window.makeKeyAndOrderFront(nil); NSApp.activate()
        let demini = frames(engine); try await wait("deminiaturized", foreground: true) { frames(engine) > demini + 5 }
        NSWorkspace.shared.notificationCenter.post(name: NSWorkspace.willSleepNotification, object: nil)
        try await stationary("sleep notification", engine, view)
        NSWorkspace.shared.notificationCenter.post(name: NSWorkspace.didWakeNotification, object: nil)
        let awakened = frames(engine); try await wait("wake notification", foreground: true) { frames(engine) > awakened + 5 }
        window.orderOut(nil); try await stationary("ordered out window", engine, view)
        window.makeKeyAndOrderFront(nil); NSApp.activate()
        let shown = frames(engine); try await wait("shown window", foreground: true) { frames(engine) > shown + 5 }
        let fullscreen = FullscreenState(); window.delegate = fullscreen
        window.toggleFullScreen(nil)
        try await wait("fullscreen", foreground: true) { fullscreen.entered }
        let full = frames(engine); try await wait("fullscreen Present", foreground: true) { frames(engine) > full + 10 }
        record("fullscreen", engine, view)
        window.toggleFullScreen(nil)
        try await wait("exit fullscreen", foreground: true) { fullscreen.exited }
        let normal = frames(engine); try await wait("windowed Present", foreground: true) { frames(engine) > normal + 10 }
        record("windowed", engine, view)
        let nextWindow = makeWindow("Auvra replacement surface", CGSize(width: 660, height: 420))
        let nextView = MetalHostView(engine: engine); nextWindow.contentView = nextView
        nextWindow.makeKeyAndOrderFront(nil); NSApp.activate()
        defer { nextView.shutdown(); nextWindow.close() }
        let replacement = frames(engine); try await wait("replacement surface", foreground: true) { frames(engine) > replacement + 5 }
        view.setFrameSize(CGSize(width: 300, height: 220)); view.needsLayout = true; view.layoutSubtreeIfNeeded(); view.shutdown()
        let afterOld = frames(engine)
        try await wait("old host cannot detach replacement", foreground: true) { frames(engine) > afterOld + 10 }
        let expectedNext = [Int((nextView.bounds.width * nextWindow.backingScaleFactor).rounded()), Int((nextView.bounds.height * nextWindow.backingScaleFactor).rounded())]
        try check(metrics(engine)["viewport_pixels"] as? [Int] == expectedNext, "Stale host resized new surface")
        record("stale host isolated", engine, nextView)
        nextView.shutdown()
        let detached = frames(engine)
        try await Task.sleep(for: .milliseconds(150)); try check(frames(engine) == detached, "Detached surface kept rendering")
        try check(renderErrors.isEmpty, "Native render failures: \(renderErrors)")
        await close(engine); try check(engine.exportMetrics() == nil, "Closed engine still owns handle")
        try await storeLifecycle()
        // Repeated teardown includes pending weak render work; last release must not queue.sync itself.
        for _ in 0..<8 {
            var controller: EngineController? = EngineController(quality: 1)
            weak var released = controller
            controller?.resize(CGSize(width: 0, height: 0), surface: 999) // rejected stale work
            controller = nil
            try await wait("controller release") { released == nil }
            released = nil
        }
    }
    func storeLifecycle() async throws {
        let counter = FactoryCounter()
        let store = StillStore(factory: {
            if counter.next() == 1 { return .failure(StillPreparationFailure(message: "Fixture: GPU unavailable")) }
            let controller = EngineController(quality: 0)
            if let error = controller.initializationError { return .failure(StillPreparationFailure(message: error)) }
            return .success(controller)
        })
        store.prepare(); try await wait("failure fallback") { store.errorMessage != nil && !store.isPreparing }
        try check(store.engine == nil, "Failed initialization installed an engine")
        store.retry(); try await wait("retry success") { store.engine != nil && !store.isPreparing }
        try check(store.errorMessage == nil, "Retry retained old error")
        store.select("frost"); try check(store.selected == "frost", "Scene unavailable after retry")
        let old = store.engine!; let oldError = old.onError
        store.retry(); try await wait("new generation") { store.engine != nil && store.engine !== old }
        oldError?("Fixture: late retired engine error")
        try await Task.sleep(for: .milliseconds(100)); try check(store.errorMessage == nil && store.selected == "frost", "Late error overwrote new engine")
        store.engine?.onError?("Fixture: rendering failed")
        try await wait("render failure fallback") { store.engine == nil && store.errorMessage != nil }
        store.retry(); try await wait("render retry") { store.engine != nil }
        await store.shutdown(); store.prepare(); store.retry()
        try check(store.engine == nil && !store.isPreparing, "Shutdown resurrected engine")
        await close(old)
        let delayed = StillStore(factory: { Thread.sleep(forTimeInterval: 0.15); return .success(EngineController(quality: 0)) })
        delayed.prepare(); await delayed.shutdown(); try await Task.sleep(for: .milliseconds(350))
        try check(delayed.engine == nil && !delayed.isPreparing, "Late creation resurrected shutdown store")
        records.append(["phase": "factory failure, retry, stale errors, canceled initialization, shutdown", "passed": true])
    }
}
private final class OpaqueCoverView: NSView {
    override var isOpaque: Bool { true }
    override func draw(_ dirtyRect: NSRect) { NSColor.black.setFill(); bounds.fill() }
}
@MainActor
private final class OccluderChecks: NSObject, NSApplicationDelegate {
    let frame: NSRect
    let ready: URL
    var window: NSWindow?
    init(frame: NSRect, ready: URL) { self.frame = frame; self.ready = ready }
    func applicationDidFinishLaunching(_ notification: Notification) {
        let cover = NSWindow(contentRect: frame, styleMask: .borderless, backing: .buffered, defer: false)
        cover.isReleasedWhenClosed = false; cover.isOpaque = true; cover.hasShadow = false
        cover.contentView = OpaqueCoverView(frame: NSRect(origin: .zero, size: frame.size))
        cover.backgroundColor = .black; cover.level = .floating
        cover.collectionBehavior = [.canJoinAllSpaces,.fullScreenAuxiliary]
        cover.orderFrontRegardless(); cover.displayIfNeeded(); window = cover
        let state: [String: Any] = ["visible":cover.isVisible,"opaque":cover.isOpaque,"frame":[cover.frame.minX,cover.frame.minY,cover.frame.width,cover.frame.height]]
        try! JSONSerialization.data(withJSONObject: state, options: [.prettyPrinted,.sortedKeys]).write(to: ready, options: .atomic)
        Task { @MainActor in try? await Task.sleep(for: .seconds(15)); NSApp.terminate(nil) }
    }
}
@main
struct StillHostSmoke {
    @MainActor static func main() {
        let app = NSApplication.shared
        if CommandLine.arguments.count == 7, CommandLine.arguments[1] == "--cover" {
            app.setActivationPolicy(.accessory)
            let numbers = CommandLine.arguments[2..<6].compactMap(Double.init)
            guard numbers.count == 4 else { exit(2) }
            let delegate = OccluderChecks(frame: NSRect(x: numbers[0],y: numbers[1],width: numbers[2],height: numbers[3]), ready: URL(fileURLWithPath: CommandLine.arguments[6]))
            app.delegate = delegate; app.run(); withExtendedLifetime(delegate) {}; return
        }
        app.setActivationPolicy(.regular)
        let directory = CommandLine.arguments.count > 1 ? URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
            : Bundle.main.bundleURL.deletingLastPathComponent().appendingPathComponent("latest-ui", isDirectory: true)
        try! FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let delegate = StillHostChecks(directory: directory); app.delegate = delegate
        app.run(); withExtendedLifetime(delegate) {}
    }
}
