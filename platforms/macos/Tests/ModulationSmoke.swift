import AppKit
import Foundation

private struct ModulationFailure: Error { let message: String }
@MainActor
private final class ModulationChecks: NSObject, NSApplicationDelegate {
    let fixture: URL
    let evidence: URL
    var records: [[String: Any]] = []
    var failures: [String] = []
    var diagnostic: (() -> [String:Any])?
    weak var fixtureWindow: NSWindow?
    weak var fixtureView: MetalHostView?
    var lastActivation: TimeInterval = 0
    init(fixture: URL, evidence: URL) { self.fixture = fixture; self.evidence = evidence }
    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { @MainActor in
            do {
                try await run(); save(); print("M9 modulation passed: real CoreAudio -> analysis -> Rust bus -> Swift -> native Metal Present; bands/transients/sample time; reset/expiry/hidden/retry/source/EOF/error and manual content preservation")
                NSApp.terminate(nil)
            } catch { failures.append(String(describing: error)); save(); print("M9 modulation failed: \(error)"); exit(1) }
        }
    }
    func save() {
        let data = try! JSONSerialization.data(withJSONObject: ["checks":records,"failures":failures], options:[.prettyPrinted,.sortedKeys])
        try! data.write(to:evidence.appendingPathComponent("native-modulation.json"),options:.atomic)
    }
    func check(_ value: @autoclosure () -> Bool, _ message: String) throws { if !value() { throw ModulationFailure(message: message) } }
    func wait(_ label: String, attempts: Int = 400, _ condition: @MainActor () -> Bool) async throws {
        for _ in 0..<attempts {
            if condition() { return }
            // Reassert only our visible fixture. Explicit hidden-host checks
            // still run with the production visibility gate and no intervention.
            if let window = fixtureWindow, let view = fixtureView,
               window.isVisible && !view.isHiddenOrHasHiddenAncestor,
               !window.occlusionState.contains(.visible),
               ProcessInfo.processInfo.systemUptime - lastActivation > 0.75 {
                lastActivation = ProcessInfo.processInfo.systemUptime
                window.orderFrontRegardless()
                let launch = NSWorkspace.OpenConfiguration(); launch.activates = true; launch.createsNewApplicationInstance = false
                NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: launch) { _, _ in }
            }
            try await Task.sleep(for:.milliseconds(25))
        }
        if var details=diagnostic?() {details["phase"]="timeout: " + label;records.append(details)}
        throw ModulationFailure(message:"Timed out: " + label)
    }
    func metrics(_ engine: EngineController) -> [String:Any] {
        guard let data=engine.exportMetrics(),let value=try? JSONSerialization.jsonObject(with:data) as? [String:Any] else { return [:] }
        return value
    }
    func frames(_ engine: EngineController) -> Int { metrics(engine)["presented_frames"] as? Int ?? 0 }
    func presented(_ engine: EngineController) -> [String:Any]? { metrics(engine)["last_presented_modulation"] as? [String:Any] }
    func number(_ value: [String:Any]?, _ key: String) -> Double { (value?[key] as? NSNumber)?.doubleValue ?? -1 }
    func record(_ phase: String, _ player: PlayerStore, _ engine: EngineController) {
        var r: [String:Any] = ["phase":phase,"state":player.snapshot?.state.rawValue ?? "none","metrics":metrics(engine)]
        if let f=player.audioAnalysis { r["analysis_generation"]=f.generation; r["sample_end"]=f.windowEndFrame; r["rms"]=f.features.rms; r["centroid_hz"]=f.features.spectralCentroidHz; r["transient"]=f.features.transient; r["drops"]=f.droppedFrames }
        records.append(r); print("Passed: \(phase)"); fflush(stdout)
    }
    func connect(_ player: PlayerStore, _ still: StillStore) {
        player.attachVisualModulation { [weak still] m in
            still?.setModulation(m.map { StillModulationInput(timeSeconds:$0.controls.timeSeconds,motion:$0.controls.motion,turbulence:$0.controls.turbulence,light:$0.controls.light) })
        }
    }
    func run() async throws {
        let player=PlayerStore(loadLibrary:false,loadSession:false,loadSpotify:false)
        let still=StillStore(); connect(player,still)
        still.prepare(); try await wait("GPU preparation") { still.engine != nil || still.errorMessage != nil }
        try check(still.errorMessage == nil,"GPU failure: \(still.errorMessage ?? "")")
        guard var engine=still.engine else { throw ModulationFailure(message:"No engine") }
        try check(still.selected == "frost" && still.supportsModulation, "Frost capability missing")
        let window=NSWindow(contentRect:NSRect(x:120,y:100,width:800,height:500),styleMask:[.titled,.closable,.resizable,.miniaturizable],backing:.buffered,defer:false)
        window.title="Auvra Music Still integration check";window.isReleasedWhenClosed=false;window.level = .floating
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        var view=MetalHostView(engine:engine);window.contentView=view;window.orderFrontRegardless();NSApp.activate()
        fixtureWindow = window; fixtureView = view
        diagnostic = { [self] in
            ["active":NSApp.isActive,"visible":window.isVisible,"occluded":!window.occlusionState.contains(.visible),
             "animating":view.isAnimating,"state":player.snapshot?.state.rawValue ?? "none",
             "position_ms":player.snapshot?.positionMs ?? 0,"analysis_epoch":player.audioAnalysis?.generation ?? 0,
             "analysis_time_ms":player.audioAnalysis?.features.playbackPositionMs ?? 0,
             "projected_modulation":player.visualModulation != nil,"store_modulation":still.modulation != nil,
             "pending_commands":player.pendingCommands.count,"error":player.errorMessage ?? "none","metrics":metrics(engine)]
        }
        defer { view.shutdown();window.close() }
        let launch = NSWorkspace.OpenConfiguration(); launch.activates = true; launch.createsNewApplicationInstance = false
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: launch) { _, error in
                if let error { continuation.resume(throwing: error) } else { continuation.resume() }
            }
        }
        print("Waiting for native Metal Present (inactive profiling override)");fflush(stdout)
        try await wait("native Metal Present", attempts:12000) { frames(engine)>3 }
        let manual=engine.exportState(); let brightness=engine.parameter("frost.brightness")
        try check(brightness != nil, "Missing Frost brightness parameter")
        player.setVolume(0.5); player.send(.setRepeat(.one)); player.openFile(fixture)
        try await wait("1 kHz real analysis and bus") {
            guard let f=player.audioAnalysis,let m=player.visualModulation else {return false}
            return abs(f.features.spectralCentroidHz-1000)<10 && m.controls.turbulence>0.2
        }
        try await wait("1 kHz reaches native Present") { number(presented(engine),"turbulence")>0.2 }
        let firstMotion=number(presented(engine),"motion")
        let firstEpoch=player.audioAnalysis!.generation
        let sampleTime=Double(player.audioAnalysis!.windowEndFrame)/Double(player.audioAnalysis!.sampleRate)
        try check(abs(number(presented(engine),"time_seconds")-sampleTime)<0.25,"Render received a different time base")
        record("1 kHz sample time / real Present",player,engine)
        player.seek(seconds:8.3)
        try await wait("bass new epoch") { player.audioAnalysis?.generation != firstEpoch && abs((player.audioAnalysis?.features.spectralCentroidHz ?? 0)-120)<10 && (player.visualModulation?.controls.motion ?? 0)>0.2 }
        try await wait("bass native motion") { number(presented(engine),"motion")>firstMotion+0.15 && number(presented(engine),"time_seconds")>8.3 }
        record("bass changes Frost motion",player,engine)
        player.seek(seconds:4.4)
        try await wait("silence release") { (player.audioAnalysis?.features.rms ?? 1)<0.00001 && number(presented(engine),"motion")>=0 && number(presented(engine),"motion")<0.005 && number(presented(engine),"light")<0.005 }
        record("silence releases measured modulation",player,engine)
        player.seek(seconds:12.3)
        try await wait("4 kHz spectral distribution") { abs((player.audioAnalysis?.features.spectralCentroidHz ?? 0)-4000)<20 && number(presented(engine),"turbulence")>0.2 && number(presented(engine),"time_seconds")>12.3 }
        record("high spectrum changes turbulence",player,engine)
        player.seek(seconds:16.2)
        try await wait("pulse segment") { (player.audioAnalysis?.features.playbackPositionMs ?? 0)>16200 }
        var lightMin=1.0,lightMax=0.0,transientMax:Float=0
        let until=ProcessInfo.processInfo.systemUptime+1.7
        while ProcessInfo.processInfo.systemUptime<until {
            if let m=player.visualModulation { lightMin=min(lightMin,Double(m.controls.light));lightMax=max(lightMax,Double(m.controls.light)) }
            transientMax=max(transientMax,player.audioAnalysis?.features.transient ?? 0)
            try await Task.sleep(for:.milliseconds(10))
        }
        try check(lightMax-lightMin>0.08 && transientMax>0.2,"Real pulse onsets did not modulate light")
        record("real bass pulse transients",player,engine)
        records.append(["phase":"pulse measurements","light_min":lightMin,"light_max":lightMax,"transient_max":transientMax])
        try check(engine.exportState()==manual && engine.parameter("frost.brightness")==brightness,"Music changed manual content")
        player.togglePlayback()
        try await wait("pause clear") { player.snapshot?.state == .paused && player.visualModulation == nil && still.modulation == nil }
        let pausedFrames=frames(engine)
        try await wait("paused native clear") { frames(engine)>pausedFrames && presented(engine)==nil }
        player.seek(seconds:8.5)
        try await wait("paused seek") { player.snapshot?.state == .paused && player.canTogglePlayback && player.snapshot?.positionMs == 8500 && player.pendingCommands.isEmpty }
        try check(player.visualModulation == nil,"Paused seek revived modulation")
        record("pause / paused seek clear",player,engine)
        player.togglePlayback()
        try await wait("resume fresh window") { player.visualModulation != nil && number(presented(engine),"time_seconds")>8.5 }
        player.attachVisualModulation { _ in } // Simulate lost host producer while audio keeps playing.
        try await wait("host expiry") { presented(engine)==nil && metrics(engine)["modulation"] is NSNull }
        record("host rejects expired controls",player,engine)
        connect(player,still)
        try await wait("producer recovery") { presented(engine) != nil }
        view.isHidden=true;try await wait("hidden host") {!view.isAnimating}
        let hiddenFrames=frames(engine)
        player.togglePlayback();try await wait("pause while hidden") {player.snapshot?.state == .paused && still.modulation == nil}
        try await Task.sleep(for:.milliseconds(200));try check(frames(engine)==hiddenFrames,"Hidden host continued presenting")
        view.isHidden=false
        try await wait("unhide has no stale influence") {frames(engine)>hiddenFrames && presented(engine)==nil}
        record("hidden pause / unhide clears before Present",player,engine)
        player.togglePlayback();try await wait("playing before retry") {player.visualModulation != nil}
        let old=engine
        still.retry();try await wait("GPU retry") {still.engine != nil && still.engine !== old}
        view.shutdown();engine=still.engine!;view=MetalHostView(engine:engine);window.contentView=view;window.orderFrontRegardless()
        fixtureView = view
        try await wait("new engine receives fresh controls") {frames(engine)>3 && presented(engine) != nil}
        try check(still.selected=="frost" && still.isModulated,"Retry lost scene or fresh modulation")
        record("retry gets current controls",player,engine)
        try check(!engine.selectScene("unsupported-scene"), "Unknown scene accepted")
        try check(still.selected == "frost" && still.isModulated, "Rejected selection interrupted Frost")
        record("Frost retains modulation after rejected selection", player, engine)
        player.setVolume(0)
        try await wait("output mute reaches zero modulation") {(player.audioAnalysis?.features.rms ?? 1)<0.00001 && number(presented(engine),"motion")>=0 && number(presented(engine),"motion")<0.005 && number(presented(engine),"light")<0.005}
        record("post-volume mute",player,engine)
        player.setVolume(0.5)
        let demo=player.demoTracks.first!
        player.play(demo)
        try await wait("silent demo gate") {player.isPlaying && player.snapshot?.capabilities.pcmAnalysis == false && player.visualModulation == nil && still.modulation == nil && presented(engine)==nil}
        record("demo cannot inherit local influence",player,engine)
        player.openFile(fixture);player.send(.setRepeat(.off));try await wait("local recovery") {player.visualModulation != nil}
        player.seek(seconds:23.7);try await wait("EOF reset") {player.snapshot?.state == .stopped && player.visualModulation == nil && presented(engine)==nil}
        record("EOF clear",player,engine)
        player.openFile(fixture.deletingLastPathComponent().appendingPathComponent("missing.wav"))
        try await wait("missing file reset") {player.snapshot?.state == .error && player.visualModulation == nil && presented(engine)==nil}
        record("error clear",player,engine)
        player.errorMessage=nil;player.openFile(fixture);try await wait("recovery for stop") {player.visualModulation != nil}
        player.stop();try await wait("stop reset") {player.snapshot?.state == .stopped && player.visualModulation == nil && presented(engine)==nil}
        record("stop clear",player,engine)
        await player.prepareToQuit();await still.shutdown()
        try check(engine.exportMetrics()==nil,"Shutdown retained graphics handle")
    }
}
@main
struct ModulationSmoke {
    @MainActor static func main() {
        let app=NSApplication.shared;app.setActivationPolicy(.regular)
        let delegate=ModulationChecks(fixture:URL(fileURLWithPath:CommandLine.arguments[1]),evidence:URL(fileURLWithPath:CommandLine.arguments[2],isDirectory:true))
        app.delegate=delegate;app.run();withExtendedLifetime(delegate){}
    }
}
