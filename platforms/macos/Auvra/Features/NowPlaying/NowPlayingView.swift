import SwiftUI

/// A listening surface. The shared store keeps transport alive while the library is hidden.
struct NowPlayingView: View {
    let player: PlayerStore
    let still: StillStore
    var navigation: AppNavigation? = nil
    var sharesPlayerChrome = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage("AuvraAnimateSpaces") private var animateSpaces = true
    private var glass = WindowGlass()
    private var paused: Bool { player.snapshot?.state == .paused && !player.isChangingTrack }

    var body: some View {
        GeometryReader { geometry in
            let coverSize = min(448, geometry.size.width * 0.42, geometry.size.height * 0.48)
            let lyricsCoverSize = min(372, geometry.size.width * 0.31, geometry.size.height * 0.44)
            ZStack {
                StillView(still: still).ignoresSafeArea()
                Color.black.opacity(paused ? 0.13 : 0).ignoresSafeArea()
                    .allowsHitTesting(false).accessibilityHidden(true)
                    .animation(AppMotion.playbackMood(reduceMotion), value: paused)
                if player.lyrics.isVisible {
                    LinearGradient(colors: [.black.opacity(0.37), .black.opacity(glass.isEnabled ? 0.10 : 0.37)],
                                   startPoint: UnitPoint(x: 0.5, y: 0.75), endPoint: .bottom)
                        .ignoresSafeArea().allowsHitTesting(false).transition(.opacity)
                }
                LinearGradient(stops: [
                    .init(color: .black.opacity(0.22), location: 0),
                    .init(color: .clear, location: 0.50),
                    .init(color: .black.opacity(glass.isEnabled ? 0.22 : 0.42), location: 1)
                ], startPoint: .top, endPoint: .bottom)
                    .ignoresSafeArea()
                    .allowsHitTesting(false)
                if player.lyrics.isVisible {
                    HStack(spacing: 48) {
                        artwork(size: lyricsCoverSize, placement: .lyrics)
                        LyricsView(player: player, lyrics: player.lyrics)
                            .environment(\.colorScheme, .dark).foregroundStyle(.white).tint(.white.opacity(0.86))
                            .frame(maxWidth: 560)
                    }.padding(.horizontal, 64).padding(.top, 44).padding(.bottom, 172)
                        .transition(.opacity)
                } else {
                    artwork(size: coverSize, placement: .center).offset(y: -28).transition(.opacity)
                }
                if !sharesPlayerChrome {
                    VStack(spacing: 0) {
                        Spacer(minLength: 0)
                        PlayerBar(player: player, navigation: navigation, immersive: true, still: still)
                    }
                }
            }.foregroundStyle(.primary)
                .animation(AppMotion.listening(reduceMotion), value: player.lyrics.isVisible)
                .animation(AppMotion.feedback(reduceMotion), value: player.lyrics.canTranslate)
        }
        .onAppear { player.setAnalysisVisible(!reduceMotion && animateSpaces) }
        .onChange(of: reduceMotion) { _, reduced in player.setAnalysisVisible(!reduced && animateSpaces) }
        .onChange(of: animateSpaces) { _, animated in player.setAnalysisVisible(animated && !reduceMotion) }
        .onDisappear { player.setAnalysisVisible(false); player.lyrics.setVisible(false) }
        .onExitCommand { navigation?.leaveNowPlaying() }
    }
    private func artwork(size: CGFloat, placement: PlayerCoverPlacement) -> some View {
        let item = player.displayedTrack
        return Button { navigation?.leaveNowPlaying() } label: {
            PlayerCover(reference: item?.artwork, placement: placement, size: size, shared: sharesPlayerChrome, paused: paused,
                        trackID: item?.id, direction: player.trackTransitionDirection)
                .contentShape(Rectangle())
        }.buttonStyle(.plain).accessibilityLabel("Back to music")
            .accessibilityIdentifier("collapse-now-playing-artwork").help("Back to music · Esc")
    }
}
