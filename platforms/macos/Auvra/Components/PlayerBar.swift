import AppKit
import SwiftUI

enum PlayerCoverPlacement: Hashable {
    case bar, center, lyrics
}

extension View {
    /// Expand the label's hit shape without enlarging its symbol.
    func playerButtonTarget(width: CGFloat = 44, height: CGFloat = 44) -> some View {
        frame(width: width, height: height).contentShape(Rectangle())
    }
}

struct PlayerCoverAnchors: PreferenceKey {
    static var defaultValue: [PlayerCoverPlacement: Anchor<CGRect>] { [:] }
    static func reduce(value: inout [PlayerCoverPlacement: Anchor<CGRect>], nextValue: () -> [PlayerCoverPlacement: Anchor<CGRect>]) {
        value.merge(nextValue(), uniquingKeysWith: { _, latest in latest })
    }
}

/// A transparent slot participates in layout while the root draws the moving cover.
/// Standalone listening surfaces can also draw their own artwork.
struct PlayerCover: View {
    let reference: String?
    let placement: PlayerCoverPlacement
    let size: CGFloat
    var shared = false
    var paused = false
    var trackID: String? = nil
    var direction = 0
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        if shared {
            Color.clear.frame(width: size, height: size)
                .anchorPreference(key: PlayerCoverAnchors.self, value: .bounds) { [placement: $0] }
                .accessibilityHidden(true)
        } else {
            PlaybackArtwork(trackID: trackID, reference: reference, direction: direction, distance: placement == .bar ? 16 : 22)
                .frame(width: size, height: size)
                .clipShape(RoundedRectangle(cornerRadius: placement == .bar ? 4 : 12))
                .shadow(color: .black.opacity(placement == .bar ? 0 : 0.25), radius: placement == .bar ? 0 : 32,
                        y: placement == .bar ? 0 : 18)
                .scaleEffect(placement != .bar && paused ? 0.945 : 1)
                .animation(AppMotion.playbackMood(reduceMotion), value: paused)
        }
    }
}

struct PlayerBar: View {
    let player: PlayerStore
    let navigation: AppNavigation?
    var sharesCover = false
    var immersive = false
    var still: StillStore? = nil
    var barLighting: StillStore? = nil
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.locale) private var locale
    private var glass = WindowGlass()

    var body: some View {
        let content = VStack(spacing: 10) {
            status
            HStack(spacing: immersive ? 22 : 10) {
                trackIdentity(player.displayedTrack)
                transport
                utilities
            }
        }
        .padding(.horizontal, immersive ? 28 : 24)
        .padding(.vertical, immersive ? 26 : 14)
        return glass.playerSurface(Rectangle(), enabled: !immersive, content: content)
        .background { compactBackdrop }
        .environment(\.colorScheme, immersive && glass.isEnabled ? .dark : colorScheme)
        .overlay(alignment: .top) {
            Rectangle().fill(AuvraColors.separator).frame(height: 1)
                .opacity(!immersive && !glass.isEnabled ? 1 : 0).allowsHitTesting(false).accessibilityHidden(true)
        }
        .animation(AppMotion.cover(reduceMotion), value: immersive)
        .animation(AppMotion.listening(reduceMotion), value: player.lyrics.isVisible)
        .animation(AppMotion.feedback(reduceMotion), value: player.lyrics.canTranslate)
    }

    private var compactBackdrop: some View {
        ZStack {
            if !glass.isEnabled { AuvraColors.playerBar }
            if !immersive, let barLighting {
                StillView(still: barLighting, compact: true)
                    .opacity(barLighting.hasPalette ? compactLightingOpacity : 0)
                    .animation(AppMotion.listening(reduceMotion), value: barLighting.hasPalette)
            }
        }
        .opacity(immersive ? 0 : 1)
        .transition(.opacity)
        .allowsHitTesting(false).accessibilityHidden(true)
    }

    private var compactLightingOpacity: Double {
        if glass.isEnabled { return colorScheme == .light ? 0.20 : 0.32 }
        return colorScheme == .light ? 0.34 : 0.72
    }

    private func trackIdentity(_ item: MediaItem?) -> some View {
        ZStack(alignment: .leading) {
            VStack(alignment: .leading, spacing: immersive ? 5 : 0) {
                Button { navigation?.leaveNowPlaying() } label: {
                    Text("AUVRA").font(.system(size: 12, weight: .medium, design: .serif)).tracking(3)
                        .frame(width: 92, height: 44, alignment: .leading).contentShape(Rectangle())
                }.buttonStyle(.plain).foregroundStyle(.white.opacity(0.72))
                    .accessibilityLabel("Back to music")
                    .accessibilityIdentifier("leave-now-playing")
                    .help("Back to music · Esc")
                    .frame(height: immersive ? 44 : 0, alignment: .bottom)
                    .clipped().opacity(immersive ? 1 : 0)
                    .allowsHitTesting(immersive)
                VStack(alignment: .leading, spacing: 4) {
                    Text(item?.title ?? "Choose a track").font(.system(size: 14, weight: .semibold)).lineLimit(1)
                        .help(item?.title ?? "").accessibilityAddTraits(.isHeader)
                    Text(item?.artist ?? "Your music").font(.system(size: 12)).lineLimit(1)
                        .foregroundStyle(immersive ? .white.opacity(0.72) : Color.secondary)
                        .help(item?.artist ?? "")
                }
                .contentTransition(.opacity)
                .animation(AppMotion.feedback(reduceMotion), value: item?.id)
            }
            .foregroundStyle(immersive ? .white : Color.primary)
            .padding(.leading, immersive ? 2 : 78).padding(.trailing, 12)
            .accessibilityHidden(!immersive)

            Button { navigation?.show(.nowPlaying) } label: {
                PlayerCover(reference: item?.artwork, placement: .bar, size: 54, shared: sharesCover,
                            trackID: item?.id, direction: player.trackTransitionDirection)
                    .padding(.leading, 12)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
            }.buttonStyle(.plain)
                .help(item.map { "Show Now Playing, \($0.title) by \($0.artist)" } ?? "Show Now Playing")
                .accessibilityLabel(item.map { "Show Now Playing, \($0.title) by \($0.artist)" } ?? "Show Now Playing")
                .accessibilityIdentifier("show-now-playing-artwork")
                .opacity(immersive ? 0 : 1)
                .allowsHitTesting(!immersive).accessibilityHidden(immersive)
        }
        .frame(width: immersive ? 212 : 248, height: immersive ? 86 : 70)
    }

    private var transport: some View {
        let content = VStack(spacing: 5) {
            TransportButtons(player: player)
            PlaybackTimeline(player: player)
                .frame(maxWidth: immersive ? 320 : 440)
        }
        .padding(.horizontal, 14).padding(.vertical, immersive ? 12 : 11)
        .frame(maxWidth: immersive ? 380 : 500)
        .frame(maxWidth: immersive ? 380 : .infinity, minHeight: immersive ? nil : 90)
        return island(cornerRadius: 21, splitsFromBar: true, content: content).frame(maxWidth: .infinity)
    }

    private var utilities: some View {
        let content = HStack(spacing: 3) {
            LyricsToggle(player: player, navigation: navigation)
            if immersive, player.lyrics.isVisible, player.lyrics.canTranslate, LyricsTranslationToggle.isAvailable(for: locale) {
                LyricsTranslationToggle(lyrics: player.lyrics).transition(.opacity)
            }
            VolumeControl(player: player)
            if let navigation { ListeningActions(player: player, navigation: navigation) }
        }
        .font(.system(size: 15, weight: .medium))
        .padding(.horizontal, 12).padding(.vertical, 11)
        .frame(minHeight: immersive ? nil : 90)
        return island(cornerRadius: 28, splitsFromBar: true, content: content)
            .fixedSize(horizontal: true, vertical: false)
    }

    @ViewBuilder private var status: some View {
        if let still, still.isPreparing {
            ProgressView("Preparing light…").controlSize(.small).tint(.white).foregroundStyle(.white)
        }
        if let still, let issue = still.errorMessage {
            island(cornerRadius: 18, content: HStack { Text(issue); Button("Try Again", action: still.retry) }
                .font(.caption).padding(10))
        }
        if let issue = player.playbackIssue ?? (immersive ? player.errorMessage : nil) {
            island(cornerRadius: 18, content: Text(issue).font(.caption).foregroundStyle(AuvraColors.warning).lineLimit(1).help(issue)
                .padding(.horizontal, 12).padding(.vertical, 6))
                .transition(.opacity)
        }
    }

    @ViewBuilder private func island(cornerRadius: CGFloat, splitsFromBar: Bool = false, content: some View) -> some View {
        let shape = RoundedRectangle(cornerRadius: splitsFromBar && !immersive ? 0 : cornerRadius)
        let fill = (colorScheme == .light ? Color.white : .black)
            .opacity(reduceTransparency ? 1 : colorScheme == .light ? 0.82 : 0.64)
        if glass.isEnabled {
            glass.playerSurface(shape, enabled: immersive, content: content)
        } else {
            content.background {
                ZStack {
                    shape.fill(AuvraColors.raised).opacity(!splitsFromBar && !immersive ? 1 : 0)
                    shape.fill(fill).opacity(immersive ? 1 : 0)
                }
                .overlay { shape.strokeBorder(AuvraColors.separator).opacity(immersive || !splitsFromBar ? 1 : 0) }
                .allowsHitTesting(false).accessibilityHidden(true)
            }
        }
    }
}

struct ListeningActions: View {
    let player: PlayerStore
    let navigation: AppNavigation
    private var glass = WindowGlass()
    @State private var queueOpen = false
    @State private var inspectedItem: MediaItem?

    var body: some View {
        HStack(spacing: 2) {
            Button { queueOpen.toggle() } label: {
                Image(systemName: "list.bullet")
                    .foregroundStyle(queueOpen ? AuvraColors.accent : Color.secondary)
                    .playerButtonTarget()
            }
            .buttonStyle(.plain)
            .accessibilityLabel(queueOpen ? "Hide queue" : "Show queue")
            .accessibilityIdentifier("show-queue")
            .help("Queue · ⌘2")
            .background(TransientPresentationReader(isPresented: $queueOpen))
            .popover(isPresented: $queueOpen, arrowEdge: .top) {
                QueueView(player: player, navigation: navigation, dismissPanel: { queueOpen = false })
                    .frame(width: 392, height: 430)
                    .background(glass.isEnabled ? Color.clear : AuvraColors.raised)
                    .accessibilityIdentifier("queue-panel")
            }

            Button { inspectedItem = player.displayedTrack } label: {
                Image(systemName: "info.circle").foregroundStyle(.secondary)
                    .playerButtonTarget()
            }
            .buttonStyle(.plain)
            .disabled(player.displayedTrack == nil)
            .accessibilityLabel("About This Track")
            .accessibilityIdentifier("about-track-button")
            .help("About This Track")
            .sheet(item: $inspectedItem) { item in
                MediaItemInspector(player: player, item: item, navigation: navigation)
            }
        }
    }
}

private struct TransientPresentationReader: NSViewRepresentable {
    @Binding var isPresented: Bool

    func makeNSView(context: Context) -> NSView { Probe() }
    func updateNSView(_ view: NSView, context: Context) {
        guard let view = view as? Probe else { return }
        view.isPresented = isPresented
        view.dismiss = { isPresented = false }
        view.updateRegistration()
    }
    static func dismantleNSView(_ view: NSView, coordinator: ()) {
        NativeTransientPresentations.shared.set(owner: view, window: nil, dismiss: nil)
    }

    private final class Probe: NSView {
        var isPresented = false
        var dismiss: (() -> Void)?
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            updateRegistration()
        }
        func updateRegistration() {
            NativeTransientPresentations.shared.set(owner: self, window: window,
                                                    dismiss: isPresented ? dismiss : nil)
        }
    }
}

struct TransportButtons: View {
    let player: PlayerStore
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        HStack(spacing: 7) {
            Button(action: player.toggleShuffle) {
                Image(systemName: "shuffle")
                    .foregroundStyle(player.shuffleEnabled ? AuvraColors.accent : Color.secondary)
                    .playerButtonTarget()
            }.disabled(!player.canShuffle).accessibilityLabel("Shuffle")
                .accessibilityValue(player.shuffleEnabled ? "On" : "Off").help("Shuffle")
            Button(action: player.previous) { Image(systemName: "backward.end.fill").playerButtonTarget() }
                .disabled(!player.canGoPrevious).accessibilityLabel("Previous track").help("Previous · ⌘←")
            Button(action: player.togglePlayback) {
                Group {
                    if player.isLoading && !player.canTogglePlayback { ProgressView().controlSize(.small) }
                    else {
                        Image(systemName: player.playbackToggleShowsPause ? "pause.fill" : "play.fill")
                            .font(.system(size: 20, weight: .medium))
                            .contentTransition(reduceMotion ? .identity : .symbolEffect(.replace))
                            .animation(reduceMotion ? nil : .easeInOut(duration: 0.15), value: player.playbackToggleShowsPause)
                    }
                }.playerButtonTarget()
            }.disabled(!player.canTogglePlayback)
                .accessibilityLabel(player.isLoading && !player.canTogglePlayback ? "Loading track" : player.playbackToggleShowsPause ? "Pause" : "Play").help("Play or pause · Space")
            Button(action: player.next) { Image(systemName: "forward.end.fill").playerButtonTarget() }
                .disabled(!player.canGoNext).accessibilityLabel("Next track").help("Next · ⌘→")
            Button(action: player.cycleRepeat) {
                Image(systemName: player.repeatMode == .one ? "repeat.1" : "repeat")
                    .foregroundStyle((player.repeatMode ?? .off) == .off ? Color.secondary : AuvraColors.accent)
                    .playerButtonTarget()
            }.disabled(!player.canRepeat).accessibilityLabel("Repeat")
                .accessibilityValue((player.repeatMode ?? .off).rawValue).help("Repeat")
        }.font(.system(size: 15, weight: .medium)).buttonStyle(.plain)
            .animation(AppMotion.feedback(reduceMotion), value: player.isLoading)
            .animation(AppMotion.feedback(reduceMotion), value: player.shuffleEnabled)
            .animation(AppMotion.feedback(reduceMotion), value: player.repeatMode)
    }
}
struct PlaybackTimeline: View {
    let player: PlayerStore
    @State private var scrubber = PlaybackScrubber()
    private var duration: Double { Double(player.displayedTrack?.durationMs ?? 0) / 1000 }
    var body: some View {
        TimelineView(.animation(minimumInterval: 0.1, paused: !player.isPlaying)) { _ in
                    let position = player.isChangingTrack ? player.displayedPosition : scrubber.position ?? player.displayedPosition
                    HStack(spacing: 10) {
                        Text(formatTime(position)).frame(width: 36, alignment: .trailing)
                        Slider(value: Binding(get: { min(position, max(1, duration)) }, set: { value in
                            if let immediate = scrubber.update(value) { player.seek(seconds: immediate) }
                        }), in: 0...max(1, duration), onEditingChanged: { editing in
                            if editing { scrubber.begin(at: player.position(), trackID: player.current?.id) }
                            else { scrubber.end(seek: player.seek(seconds:)) }
                        })
                        .disabled(!player.canSeek).accessibilityLabel("Playback position")
                        .accessibilityValue("\(formatTime(position)) of \(formatTime(duration))")
                        Text(formatTime(duration)).frame(width: 36, alignment: .leading)
                    }.font(.caption2.monospacedDigit()).foregroundStyle(.secondary)
                }
        .onChange(of: player.displayedTrack?.id) { scrubber.cancel() }
        .onChange(of: player.isChangingTrack) { if player.isChangingTrack { scrubber.cancel() } }
        .onChange(of: player.snapshot?.revision) { scrubber.reconcile(player.snapshot) }
        .onChange(of: player.latestPositionRequest) { _, request in scrubber.supersede(with: request) }
        .onChange(of: player.playbackIssue) { if player.playbackIssue != nil { scrubber.cancel() } }
        .onChange(of: player.errorMessage) { if player.errorMessage != nil { scrubber.cancel() } }
    }
}
struct VolumeControl: View {
    let player: PlayerStore
    var body: some View {
        HStack(spacing: 4) {
                VolumeMuteButton(player: player, targetSize: 44)
                VolumeSlider(player: player)
            }.disabled(player.snapshot?.capabilities.volume != true)
    }
}
struct VolumeSlider: View {
    let player: PlayerStore
    var width: CGFloat = 84
    var accessibilityName = "Volume"
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ZStack {
            if let volume = player.displayedVolume {
                Slider(value: Binding(get: { player.displayedVolume ?? volume }, set: { player.setVolume($0) }), in: 0...1)
                    .disabled(player.snapshot?.capabilities.volume != true)
                    .accessibilityLabel(accessibilityName)
                    .accessibilityValue("\(Int((volume * 100).rounded())) percent")
                    .transition(reduceMotion ? .identity : .opacity)
            } else {
                Capsule().fill(.secondary.opacity(0.14)).frame(height: 4)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(accessibilityName)
                    .accessibilityValue("Unavailable")
                    .transition(reduceMotion ? .identity : .opacity)
            }
        }
        .frame(width: width, height: 20)
        .animation(AppMotion.feedback(reduceMotion), value: player.displayedVolume != nil)
    }
}
struct VolumeMuteButton: View {
    let player: PlayerStore
    var targetSize: CGFloat = 22
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    private var label: String {
        guard let volume = player.displayedVolume else { return "Volume unavailable" }
        return volume == 0 ? "Unmute" : "Mute"
    }

    var body: some View {
        Button(action: player.toggleMute) {
            Image(systemName: player.displayedVolume.map { $0 == 0 ? "speaker.slash" : "speaker.wave.2" } ?? "speaker")
                .contentTransition(reduceMotion ? .identity : .symbolEffect(.replace))
                .playerButtonTarget(width: targetSize, height: targetSize)
        }.buttonStyle(.plain).foregroundStyle(.secondary)
            .disabled(player.displayedVolume == nil || player.snapshot?.capabilities.volume != true)
            .accessibilityLabel(label)
            .help(label)
            .animation(AppMotion.feedback(reduceMotion), value: player.isMuted)
            .animation(AppMotion.feedback(reduceMotion), value: player.displayedVolume != nil)
    }
}
