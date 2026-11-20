import SwiftUI

/// A native placeholder for the demo collection, shared by library, rows and player.
struct CollectionArtwork: View {
    var body: some View {
        GeometryReader { geometry in
            ZStack {
                LinearGradient(colors: [Color(red: 0.20, green: 0.43, blue: 0.43),
                                        Color(red: 0.10, green: 0.16, blue: 0.25)],
                               startPoint: .topLeading, endPoint: .bottomTrailing)
                Circle().fill(.white.opacity(0.06))
                    .frame(width: geometry.size.width * 0.8)
                    .offset(x: geometry.size.width * 0.18, y: -geometry.size.height * 0.22)
                Image(systemName: "sun.horizon")
                    .font(.system(size: max(16, geometry.size.width * 0.34), weight: .ultraLight))
                    .foregroundStyle(.white.opacity(0.75))
            }
        }.aspectRatio(1, contentMode: .fit).clipped().accessibilityHidden(true)
    }
}

/// A page owns its avatar palette independently of the playing track.
/// Thumbnail fetches and palette extraction share the bounded artwork service.
@MainActor
final class ArtistBannerLighting {
    let still: StillStore
    private var generation: UInt64 = 0
    private let loader: @MainActor (String) async -> ArtworkPalette?

    init(still: StillStore? = nil,
         loader: @escaping @MainActor (String) async -> ArtworkPalette? = { await LocalArtworkLoader.shared.palette(for: $0) }) {
        self.still = still ?? StillStore(compact: true)
        self.loader = loader
    }

    func load(_ reference: String?) async {
        guard !Task.isCancelled else { return }
        generation &+= 1
        let token = generation
        guard let reference else { still.setPalette(nil); return }
        // Retain the previous supplied palette while decoding, then let Frost
        // interpolate to the matching avatar. Never expose its builtin preset.
        let palette = await loader(reference)
        guard !Task.isCancelled, generation == token else { return }
        still.setPalette(palette?.colors)
    }

    func cancel() { generation &+= 1 }

    nonisolated static func isVisible(size: CGSize, scrollBounds: CGRect?) -> Bool {
        guard size.width.isFinite, size.height.isFinite, size.width > 0, size.height > 0,
              let scrollBounds, !scrollBounds.isNull, !scrollBounds.isInfinite,
              scrollBounds.width > 0, scrollBounds.height > 0 else { return false }
        let overlap = CGRect(origin: .zero, size: size).intersection(scrollBounds)
        return !overlap.isNull && overlap.width > 0 && overlap.height > 0
    }
}

/// Frost is mounted only while the artist hero intersects its scroll viewport.
/// Offscreen pages keep their palette without a display link or GPU surface.
struct ArtistAmbientBanner<Content: View>: View {
    let artworkReference: String?
    private let content: Content
    @State private var lighting: ArtistBannerLighting
    @State private var visible = false

    init(artworkReference: String?, lighting: ArtistBannerLighting? = nil, @ViewBuilder content: () -> Content) {
        self.artworkReference = artworkReference
        self._lighting = State(initialValue: lighting ?? ArtistBannerLighting())
        self.content = content()
    }

    var body: some View {
        content
            .padding(28)
            .frame(maxWidth: .infinity, minHeight: 268, alignment: .leading)
            .background {
                ZStack {
                    Color(white: 0.12)
                    if visible {
                        StillView(still: lighting.still, compact: true)
                    } else {
                        LinearGradient(colors: lighting.still.displayPalette.map {
                            Color(red: Double($0[0]), green: Double($0[1]), blue: Double($0[2]))
                        }, startPoint: .topLeading, endPoint: .bottomTrailing)
                    }
                    Color.black.opacity(0.12)
                    // Keep the avatar side vivid while protecting white text
                    // even when all three avatar colors are near white.
                    LinearGradient(stops: [
                        .init(color: .clear, location: 0),
                        .init(color: .black.opacity(0.52), location: 0.20),
                        .init(color: .black.opacity(0.58), location: 0.26),
                        .init(color: .black.opacity(0.58), location: 1)
                    ], startPoint: .leading, endPoint: .trailing)
                }.allowsHitTesting(false).accessibilityHidden(true)
            }
            .clipShape(RoundedRectangle(cornerRadius: 24))
            .overlay {
                RoundedRectangle(cornerRadius: 24).strokeBorder(.white.opacity(0.08), lineWidth: 1)
                    .allowsHitTesting(false).accessibilityHidden(true)
            }
            .environment(\.colorScheme, .dark)
            .onGeometryChange(for: Bool.self) { geometry in
                ArtistBannerLighting.isVisible(size: geometry.size,
                    scrollBounds: geometry.bounds(of: .scrollView(axis: .vertical)))
            } action: { visible = $0 }
            .task(id: artworkReference) { await lighting.load(artworkReference) }
            .onDisappear { visible = false; lighting.cancel() }
    }
}

struct DemoBadge: View {
    var body: some View {
        Label("Silent demo", systemImage: "speaker.slash")
            .font(.caption).foregroundStyle(.secondary)
            .padding(.horizontal, 10).padding(.vertical, 5)
            .background(AuvraColors.subtle, in: Capsule())
            .accessibilityLabel("Silent demo collection. These tracks contain no audio.")
    }
}
