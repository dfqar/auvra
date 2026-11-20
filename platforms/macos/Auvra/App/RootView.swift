import SwiftUI

struct RootView: View {
    @Bindable var player: PlayerStore
    @Bindable var navigation: AppNavigation
    let still: StillStore
    let windows: MacWindowService
    var barLighting: StillStore? = nil
    @Environment(\.openWindow) private var openWindow
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var sidebarVisibility: NavigationSplitViewVisibility = .all
    @State private var playerBarHeight: CGFloat = 0
    private var glass = WindowGlass()
    private var pageKey: String { navigation.page.rawValue + ":" + (navigation.selectedCollection?.uri ?? "") }
    private var scrollsBehindBar: Bool { glass.isEnabled && player.errorMessage == nil }

    var body: some View {
        let immersive = navigation.page == .nowPlaying
        let coverPlacement: PlayerCoverPlacement = immersive ? (player.lyrics.isVisible ? .lyrics : .center) : .bar
        let item = player.displayedTrack
        let coverPaused = player.snapshot?.state == .paused && !player.isChangingTrack
        ZStack(alignment: .bottom) {
            if immersive {
                NowPlayingView(player: player, still: still, navigation: navigation, sharesPlayerChrome: true)
                    .ignoresSafeArea(.container, edges: .top)
                    .transition(.opacity)
            } else {
                VStack(spacing: 0) {
                    browser.padding(.top, 10).transition(.opacity)
                    RecoveryBanner(player: player)
                }
                .padding(.bottom, scrollsBehindBar ? 0 : playerBarHeight)
            }
            // Keep the controls, scrubber and panels mounted across both surfaces.
            PlayerBar(player: player, navigation: navigation, sharesCover: true, immersive: immersive,
                      still: immersive ? still : nil, barLighting: barLighting)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { playerBarHeight = $0 }
        }
        .animation(AppMotion.listening(reduceMotion), value: immersive)
        .animation(AppMotion.feedback(reduceMotion), value: player.errorMessage)
        .frame(minWidth: 960, minHeight: 650)
        .overlayPreferenceValue(PlayerCoverAnchors.self) { anchors in
            GeometryReader { geometry in
                PlayerCoverOverlay(reference: item?.artwork, trackID: item?.id, direction: player.trackTransitionDirection,
                                   paused: coverPaused, target: anchors[coverPlacement].map {
                    PlayerCoverTarget(placement: coverPlacement, frame: geometry[$0])
                })
            }.allowsHitTesting(false).accessibilityHidden(true)
        }
        .background {
            if !immersive {
                AuvraColors.background.ignoresSafeArea(.container, edges: .top)
            }
        }
        .background(MainWindowReader(found: windows.registerMain))
        .onChange(of: player.spotifyEditPending) { old, pending in
            if old && !pending, player.spotifyEditIssue == nil, let update = player.spotifyCreatedPlaylist { navigation.refreshCollection(update) }
        }
        .onAppear { windows.connect(player: player, navigation: navigation, openMain: { openWindow(id: "main") }) }
        .toolbarBackground(.hidden, for: .windowToolbar)
        .auvraAppearance()
    }

    private var browser: some View {
        NavigationSplitView(columnVisibility: $sidebarVisibility) {
            VStack(alignment: .leading, spacing: 12) {
                sidebarHeader(collapsed: false)
                    .padding(.horizontal, 16).padding(.top, 24).padding(.bottom, 8)
                List(selection: $navigation.selection) {
                    Section {
                        ForEach([AppPage.search, .home, .library]) { page in
                            sidebarItem(page).tag(page).padding(.vertical, 4)
                        }
                    } header: { Text("Your Spotify").foregroundStyle(AuvraColors.sidebarHeading) }
                    Section {
                        sidebarItem(.localLibrary).tag(AppPage.localLibrary).padding(.vertical, 4)
                    } header: { Text("On this Mac").foregroundStyle(AuvraColors.sidebarHeading) }
                }.listStyle(.sidebar)
            }
            .background(glass.isEnabled ? Color.clear : AuvraColors.sidebar)
            .navigationSplitViewColumnWidth(min: 180, ideal: 200, max: 240)
            .toolbar(removing: .sidebarToggle)
        } detail: {
            VStack(spacing: 0) {
                if sidebarVisibility == .detailOnly {
                    sidebarHeader(collapsed: true)
                        .padding(.horizontal, 20).padding(.top, 24).padding(.bottom, 8)
                }
                Group {
                    switch navigation.page {
                    case .home: SpotifyHomeView(player: player, navigation: navigation)
                    case .library: SpotifyLibraryView(player: player, navigation: navigation)
                    case .likedSongs: SpotifyLikedSongsView(player: player, navigation: navigation)
                    case .search: SearchView(player: player, navigation: navigation)
                    case .queue: QueueView(player: player, navigation: navigation)
                    case .nowPlaying: EmptyView()
                    case .localLibrary: LibraryView(player: player)
                    case .collection: SpotifyCollectionView(player: player, navigation: navigation)
                    }
                }.id(pageKey).transition(.opacity)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    // Let rows travel under the glass and clear the bar at the end of the scroll.
                    .contentMargins(.bottom, scrollsBehindBar ? playerBarHeight : 0, for: .scrollContent)
            }
            .background(AuvraColors.background)
        }
        .animation(AppMotion.navigation(reduceMotion), value: pageKey)
    }

    private func sidebarHeader(collapsed: Bool) -> some View {
        let action = collapsed ? "Show Sidebar" : "Hide Sidebar"
        return HStack(spacing: 8) {
            ViewThatFits(in: .horizontal) {
                Text("AUVRA")
                    .font(.system(size: 22, weight: .medium, design: .serif)).tracking(4)
                    .fixedSize()
                Text("AUVRA")
                    .font(.system(size: 16, weight: .medium, design: .serif)).tracking(2)
                    .fixedSize()
            }
            if !collapsed { Spacer(minLength: 0) }
            Button { sidebarVisibility = collapsed ? .all : .detailOnly } label: {
                Image(systemName: "sidebar.left")
                    .frame(width: 32, height: 32)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(action)
            .help(action)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func sidebarItem(_ page: AppPage) -> some View {
        HStack(spacing: 10) {
            Image(systemName: page.symbol).frame(width: 18).accessibilityHidden(true)
            Text(page.title)
        }.foregroundStyle(Color.primary.opacity(navigation.selection == page ? 1 : 0.82))
    }
}

private struct PlayerCoverTarget: Equatable {
    let placement: PlayerCoverPlacement
    let frame: CGRect
}

/// One cover stays mounted above both pages. Its target can briefly disappear while
/// SwiftUI lays out the incoming page, so hold the last valid rectangle in that gap.
private struct PlayerCoverOverlay: View {
    let reference: String?
    let trackID: String?
    let direction: Int
    let paused: Bool
    let target: PlayerCoverTarget?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var displayed: PlayerCoverTarget?

    var body: some View {
        Color.clear.overlay {
            if let cover = displayed ?? target {
                PlayerCover(reference: reference, placement: cover.placement, size: cover.frame.width, paused: paused,
                            trackID: trackID, direction: direction)
                    .position(x: cover.frame.midX, y: cover.frame.midY)
            }
        }.onChange(of: target, initial: true) { _, next in
            guard let next, !next.frame.isEmpty, next.frame.width.isFinite, next.frame.height.isFinite else { return }
            if let displayed, displayed.placement != next.placement {
                withAnimation(AppMotion.cover(reduceMotion)) { self.displayed = next }
            } else { displayed = next }
        }
    }
}
