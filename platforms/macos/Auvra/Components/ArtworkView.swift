import SwiftUI
import Observation

@MainActor @Observable
final class ArtworkPresentation {
    private(set) var image: NSImage?
    private(set) var reference: String?
    @ObservationIgnored private var generation: UInt64 = 0
    @ObservationIgnored private let loader: @MainActor (String) async -> NSImage?
    init(loader: @escaping @MainActor (String) async -> NSImage? = { await LocalArtworkLoader.shared.image(for: $0) }) { self.loader = loader }
    static func allowsTransition(from: String?, to: String?, preference: Bool, reduceMotion: Bool) -> Bool {
        preference && !reduceMotion && to?.hasPrefix("/") == true && (from == nil || from?.hasPrefix("/") == true)
    }
    func cancel() { generation &+= 1; image = nil; reference = nil }
    func load(_ next: String?, preference: Bool, reduceMotion: Bool, replacesImmediately: Bool = false) async {
        generation &+= 1; let token = generation
        let retain = !replacesImmediately && Self.allowsTransition(from: reference, to: next, preference: preference, reduceMotion: reduceMotion)
        let animate = retain || (replacesImmediately && preference && !reduceMotion)
        if !retain { image = nil; reference = nil }
        guard let next else { image = nil; reference = nil; return }
        // An old local cover can survive for at most 120 ms, including slow/cancelled loads.
        let expiry = retain ? Task { @MainActor [weak self] in
            do { try await Task.sleep(for: .milliseconds(120)) } catch { return }
            guard let self, !Task.isCancelled, generation == token else { return }
            image = nil; reference = nil
        } : nil
        defer { expiry?.cancel() }
        let loaded = await loader(next)
        guard !Task.isCancelled, generation == token else { return }
        withAnimation(animate ? .easeInOut(duration: 0.18) : nil) { image = loaded; reference = next }
    }
}

struct ArtworkView: View {
    fileprivate static let playbackMatte = Color(white: 0.10)
    let reference: String?
    var isPlaybackLayer = false
    @AppStorage("AuvraArtworkTransitions") private var transitions = true
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var presentation = ArtworkPresentation()
    var body: some View {
        GeometryReader { geometry in
            ZStack {
                if !isPlaybackLayer || presentation.image == nil { CollectionArtwork().transition(.opacity) }
                if let image = presentation.image {
                    if isPlaybackLayer { Self.playbackMatte.transition(.opacity) }
                    Image(nsImage: image).resizable().scaledToFit()
                        .id(presentation.reference)
                        // Keep image replacement separate from the parent cover's geometry.
                        .transition(reduceMotion || (!isPlaybackLayer && presentation.reference?.hasPrefix("https://") == true) ? .identity : .opacity)
                }
            }.frame(width: geometry.size.width, height: geometry.size.height).clipped()
        }.aspectRatio(1, contentMode: .fit).clipped()
            .task(id: reference) {
                await presentation.load(reference, preference: transitions, reduceMotion: reduceMotion, replacesImmediately: isPlaybackLayer)
            }
            .onDisappear { presentation.cancel() }.accessibilityHidden(true)
    }
}

/// Track art moves within the cover's bounds, independently of the cover moving between screens.
struct PlaybackArtwork: View {
    let trackID: String?
    let reference: String?
    let direction: Int
    let distance: CGFloat
    @AppStorage("AuvraArtworkTransitions") private var transitions = true
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var layers: [Layer] = []
    @State private var motion: Motion?

    private struct Selection: Equatable {
        let id: String?
        let reference: String?
    }

    private struct Layer: Identifiable {
        var selection: Selection
        var offset: CGFloat = 0
        var opacity: Double = 1
        var id: String? { selection.id }
    }

    private struct Motion {
        let id = UUID()
        let destination: String
        let distance: CGFloat
    }

    var body: some View {
        let selection = Selection(id: trackID, reference: reference)
        ZStack {
            ArtworkView.playbackMatte
            ForEach(layers.isEmpty ? [Layer(selection: selection)] : layers) { layer in
                ArtworkView(reference: layer.selection.reference, isPlaybackLayer: true)
                    .offset(x: layer.offset)
                    .opacity(layer.opacity)
                    .transition(.identity)
            }
        }
        .clipped()
        .onChange(of: selection, initial: true) { _, next in retarget(to: next) }
        .task(id: motion?.id) {
            guard let next = motion else { return }
            await Task.yield()
            guard !Task.isCancelled, motion?.id == next.id else { return }
            withAnimation(.easeInOut(duration: 0.24), completionCriteria: .removed) {
                layers = layers.map { layer in
                    let arriving = layer.id == next.destination
                    return Layer(selection: layer.selection, offset: arriving ? 0 : -next.distance,
                                 opacity: arriving ? 1 : 0)
                }
            } completion: {
                guard motion?.id == next.id else { return }
                layers.removeAll { $0.id != next.destination }
                motion = nil
            }
        }
        .onChange(of: reduceMotion) { if reduceMotion { settle(on: selection) } }
        .onChange(of: transitions) { if !transitions { settle(on: selection) } }
    }

    private func retarget(to selection: Selection) {
        let previous = motion?.destination ?? layers.last?.id
        let retained = layers.firstIndex { $0.id == selection.id }
        if previous == selection.id, let retained {
            layers[retained].selection = selection
            return
        }
        guard transitions, !reduceMotion, let previous, let destination = selection.id else {
            settle(on: selection)
            return
        }
        let offset = CGFloat(direction.signum()) * distance
        var transaction = Transaction(animation: nil)
        transaction.disablesAnimations = true
        withTransaction(transaction) {
            // Only a genuinely new layer starts at the edge. Retained identities
            // keep their presentation values until SwiftUI animates to the new targets.
            if let retained {
                layers[retained].selection = selection
            } else {
                if layers.count == 4, let oldest = layers.firstIndex(where: { $0.id != previous }) {
                    layers.remove(at: oldest)
                }
                layers.append(Layer(selection: selection, offset: offset, opacity: 0))
            }
            motion = Motion(destination: destination, distance: offset)
        }
    }

    private func settle(on selection: Selection) {
        var transaction = Transaction(animation: nil)
        transaction.disablesAnimations = true
        withTransaction(transaction) { motion = nil; layers = [Layer(selection: selection)] }
    }
}
