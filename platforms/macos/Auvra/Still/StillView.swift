import SwiftUI

struct StillPreparationFailure: Error, Sendable { let message: String }

@MainActor @Observable
final class StillStore {
    typealias Factory = @Sendable () -> Result<EngineController, StillPreparationFailure>
    private(set) var engine: EngineController?
    private(set) var scenes: [ScenePresentation] = []
    private(set) var selected = "frost"
    private(set) var errorMessage: String?
    private(set) var isPreparing = false
    private(set) var paletteRevision: UInt64 = 0
    private(set) var modulation: StillModulationInput?
    @ObservationIgnored private var modulationDeadline: TimeInterval = 0
    var supportsModulation: Bool { scenes.first(where: { $0.id == selected })?.supportsModulation == true }
    var isModulated: Bool { supportsModulation && modulation != nil && ProcessInfo.processInfo.systemUptime < modulationDeadline }
    @ObservationIgnored private var stopped = false
    @ObservationIgnored private var generation: UInt64 = 0
    @ObservationIgnored private var preparation: Task<Void, Never>?
    @ObservationIgnored private let factory: Factory
    @ObservationIgnored private let compact: Bool
    private var palette: [[Float]]?
    // A missing cover is neutral. Auvra never releases into a scene preset.
    var displayPalette: [[Float]] { palette ?? [[0.16,0.16,0.16],[0.22,0.22,0.22],[0.12,0.12,0.12]] }
    var hasPalette: Bool { palette != nil }


    init(compact: Bool = false, factory: Factory? = nil) {
        self.compact = compact
        self.factory = factory ?? {
            let controller = EngineController(quality: compact ? 1 : 0)
            if let message = controller.initializationError { return .failure(StillPreparationFailure(message: message)) }
            return .success(controller)
        }
    }

    /// Device creation stays off MainActor. Navigation and playback do not depend on it.
    func prepare() {
        guard !stopped, engine == nil, !isPreparing, errorMessage == nil else { return }
        generation &+= 1
        let token = generation
        let factory = self.factory
        isPreparing = true
        preparation = Task { [weak self] in
            let result = await Task.detached(priority: .userInitiated, operation: factory).value
            guard let self, generation == token, !Task.isCancelled else { return }
            preparation = nil; isPreparing = false
            switch result {
            case .failure(let error): errorMessage = error.message
            case .success(let controller):
                controller.onError = { [weak self] message in
                    Task { @MainActor in self?.fail(message, generation: token) }
                }
                scenes = controller.sceneCatalog().scenes
                guard controller.selectScene(selected) else {
                    errorMessage = "Frost could not be loaded."
                    return
                }
                if compact, selected == "frost" {
                    controller.setParameters([
                        ("frost.brightness", 1.25), ("frost.diffusion", 0.78), ("frost.grain", 0.14),
                        ("frost.light1.x", 0.16), ("frost.light1.y", 0.38), ("frost.light1.width", 1.2), ("frost.light1.rotation", 0),
                        ("frost.light2.x", 0.54), ("frost.light2.y", 0.65), ("frost.light2.width", 1.2), ("frost.light2.rotation", 0),
                        ("frost.light3.x", 0.87), ("frost.light3.y", 0.37), ("frost.light3.width", 1.2), ("frost.light3.power", 0.72)
                    ])
                }
                // Seed the pending palette before publishing a renderable engine.
                applyPalette(to: controller)
                controller.updateModulation(modulation, expiresAt: modulationDeadline)
                engine = controller
            }
        }
    }
    func retry() {
        guard !stopped else { return }
        generation &+= 1
        preparation?.cancel(); preparation = nil
        engine = nil; errorMessage = nil; isPreparing = false
        prepare()
    }
    func shutdown() async {
        modulation = nil; modulationDeadline = 0
        stopped = true; generation &+= 1
        preparation?.cancel(); preparation = nil; isPreparing = false
        let controller = engine; engine = nil
        if let controller {
            await withCheckedContinuation { continuation in controller.close { continuation.resume() } }
        }
    }
    deinit { preparation?.cancel() }
    private func fail(_ message: String, generation token: UInt64) {
        guard generation == token else { return }
        errorMessage = message; engine = nil; isPreparing = false
    }
    func select(_ scene: String) {
        if engine?.selectScene(scene) == true { selected = scene }
    }
    func setModulation(_ input: StillModulationInput?) {
        guard !stopped else { return }
        modulation = input; modulationDeadline = ProcessInfo.processInfo.systemUptime + 0.3
        engine?.updateModulation(input, expiresAt: modulationDeadline)
    }
    func setPalette(_ value: [[Float]]?) {
        guard value == nil || value?.count == 3 && value!.allSatisfy({ $0.count == 3 && $0.allSatisfy { $0.isFinite && (0...1).contains($0) } }) else { return }
        guard palette != value else { return }
        palette = value; applyPalette(); paletteRevision &+= 1
    }
    private func applyPalette(to controller: EngineController? = nil) {
        let controller = controller ?? engine
        controller?.configureAmbientSmoothing(colorSeconds: 0.85, attackSeconds: 0.08, releaseSeconds: 0.45, transformSeconds: 0.85)
        controller?.updateAmbient(colors: displayPalette.map { ($0[0], $0[1], $0[2]) })
    }
}

struct StillView: View {
    let still: StillStore
    var compact = false
    @AppStorage("AuvraAnimateSpaces") private var animateSpaces = true
    @AppStorage("AuvraStillFrameRate") private var frameRate = 60
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        Group {
            if animateSpaces && !reduceMotion && (!compact || still.hasPalette), let engine = still.engine {
                let tone = still.displayPalette[0]
                let placeholder = CGColor(srgbRed: CGFloat(tone[0]), green: CGFloat(tone[1]), blue: CGFloat(tone[2]), alpha: 1)
                MetalCanvas(engine: engine, frameRatePreference: compact ? 30 : (frameRate == 0 ? 0 : 60), powerPolicy: 0,
                            activity: {}, becameInactive: {}, contentRevision: still.paletteRevision, placeholderColor: placeholder)
                    .id(ObjectIdentifier(engine)).accessibilityHidden(true)
            } else {
                let colors = still.displayPalette.map { Color(red: Double($0[0]), green: Double($0[1]), blue: Double($0[2])) }
                Group {
                    if compact {
                        ZStack {
                            ForEach(colors.indices, id: \.self) { index in
                                RadialGradient(colors: [colors[index], .clear],
                                               center: UnitPoint(x: 0.15 + Double(index) * 0.35, y: index == 1 ? 0.8 : 0.2),
                                               startRadius: 12, endRadius: 220)
                            }
                        }
                    } else {
                        LinearGradient(colors: colors, startPoint: .topLeading, endPoint: .bottomTrailing)
                    }
                }.id(still.paletteRevision).transition(.opacity).accessibilityHidden(true)
            }
        }.animation(reduceMotion ? nil : .easeInOut(duration: 0.85), value: still.paletteRevision)
            .onChange(of: shouldPrepare, initial: true) { _, ready in if ready { still.prepare() } }
    }

    private var shouldPrepare: Bool { animateSpaces && !reduceMotion && (!compact || still.hasPalette) }
}
