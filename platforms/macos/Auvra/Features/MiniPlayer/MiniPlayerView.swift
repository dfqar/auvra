import SwiftUI

struct MiniPlayerView: View {
    let player: PlayerStore
    let showMain: () -> Void
    @State private var scrubber = PlaybackScrubber()
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    private var glass = WindowGlass()
    private var duration: Double { Double(player.displayedTrack?.durationMs ?? 0) / 1000 }
    var body: some View {
        let item = player.displayedTrack
        VStack(spacing: 12) {
            HStack(spacing: 12) {
                PlaybackArtwork(trackID: item?.id, reference: item?.artwork, direction: player.trackTransitionDirection, distance: 16)
                    .frame(width: 64, height: 64).clipShape(RoundedRectangle(cornerRadius: 4))
                VStack(alignment: .leading, spacing: 5) {
                    Text(item?.title ?? "Your music, nearby.").font(.callout.weight(.medium)).lineLimit(2).help(item?.title ?? "")
                    Text(item?.artist ?? "Choose a track in the main player").font(.caption).foregroundStyle(.secondary).lineLimit(1).help(item?.artist ?? "")
                }.frame(maxWidth: .infinity, alignment: .leading)
                    .contentTransition(.opacity).animation(AppMotion.navigation(reduceMotion), value: item?.id)
                Button(action: showMain) { Image(systemName: "arrow.up.left.and.arrow.down.right") }
                    .buttonStyle(.plain).accessibilityLabel("Show Main Player").help("Show Main Player · ⌘0")
            }
            TimelineView(.animation(minimumInterval: 0.1, paused: !player.isPlaying)) { _ in
                let position = player.isChangingTrack ? player.displayedPosition : scrubber.position ?? player.displayedPosition
                HStack(spacing: 8) {
                    Text(formatTime(position)).frame(width: 32, alignment: .trailing)
                    Slider(value: Binding(get: { min(position, max(1, duration)) }, set: { value in
                        if let immediate = scrubber.update(value) { player.seek(seconds: immediate) }
                    }), in: 0...max(1, duration), onEditingChanged: { editing in
                        if editing { scrubber.begin(at: player.position(), trackID: player.current?.id) }
                        else { scrubber.end(seek: player.seek(seconds:)) }
                    }).disabled(!player.canSeek).accessibilityLabel("Mini playback position")
                        .accessibilityValue("\(formatTime(position)) of \(formatTime(duration))")
                    Text(formatTime(duration)).frame(width: 32, alignment: .leading)
                }.font(.caption2.monospacedDigit()).foregroundStyle(.secondary)
            }
            glass.container(spacing: 0) { controls }
            HStack {
              Group {
                if player.errorMessage != nil { RecoveryBanner(player: player, compact: true) }
                else if let issue = player.playbackIssue { Text(issue).foregroundStyle(AuvraColors.warning) }
                else if player.canEditQueue, let issue = player.snapshot?.session.issue { Text(issue.message).foregroundStyle(AuvraColors.warning) }
                else if let current = player.current, let issue = player.queueIssue(current.id) { Text(issue).foregroundStyle(AuvraColors.warning) }
            }.font(.caption2).lineLimit(2).frame(maxWidth: .infinity, minHeight: 24, alignment: .leading)
            }
        }.padding(16).frame(width: 360, height: 264)
            .background(AuvraColors.background).auvraAppearance()
            .animation(AppMotion.feedback(reduceMotion), value: player.isLoading)
            .animation(AppMotion.feedback(reduceMotion), value: player.errorMessage)
            .onChange(of: item?.id) { scrubber.cancel() }
            .onChange(of: player.isChangingTrack) { if player.isChangingTrack { scrubber.cancel() } }
            .onChange(of: player.snapshot?.revision) { scrubber.reconcile(player.snapshot) }
            .onChange(of: player.latestPositionRequest) { _, request in scrubber.supersede(with: request) }
            .onChange(of: player.playbackIssue) { if player.playbackIssue != nil { scrubber.cancel() } }
            .onChange(of: player.errorMessage) { if player.errorMessage != nil { scrubber.cancel() } }
    }

    private var controls: some View {
        let transport = HStack(spacing: 22) {
                Button(action: player.previous) { Image(systemName: "backward.end.fill").frame(width: 28, height: 28) }
                    .disabled(!player.canGoPrevious).accessibilityLabel("Previous track")
                Button(action: player.togglePlayback) {
                    Group {
                        if player.isLoading && !player.canTogglePlayback { ProgressView().controlSize(.small) }
                        else { Image(systemName: player.playbackToggleShowsPause ? "pause.fill" : "play.fill").font(.title3)
                                .contentTransition(reduceMotion ? .identity : .symbolEffect(.replace))
                                .animation(reduceMotion ? nil : .easeInOut(duration: 0.15), value: player.playbackToggleShowsPause) }
                    }.frame(width: 36, height: 32)
                }.disabled(!player.canTogglePlayback)
                    .accessibilityLabel(player.isLoading && !player.canTogglePlayback ? "Loading track" : player.playbackToggleShowsPause ? "Pause" : "Play")
                Button(action: player.next) { Image(systemName: "forward.end.fill").frame(width: 28, height: 28) }
                    .disabled(!player.canGoNext).accessibilityLabel("Next track")
        }
        .padding(.horizontal, glass.isEnabled ? 8 : 0)
        .frame(height: glass.isEnabled ? 40 : 32)
        let volume = HStack(spacing: glass.isEnabled ? 12 : 22) {
            VolumeMuteButton(player: player)
            VolumeSlider(player: player, width: 70, accessibilityName: "Mini volume")
        }
        .padding(.horizontal, glass.isEnabled ? 10 : 0)
        .frame(height: glass.isEnabled ? 40 : 32)
        return HStack(spacing: 0) {
            glass.surface(Capsule(), content: transport)
            Spacer(minLength: 0)
            glass.surface(Capsule(), content: volume)
        }.buttonStyle(.plain)
    }
}


struct RecoveryBanner: View {
    let player: PlayerStore
    var compact = false
    var body: some View {
        if let message = player.errorMessage {
            HStack(alignment: .center, spacing: 12) {
                Image(systemName: "exclamationmark.triangle").accessibilityHidden(true)
                Text(message).font(.caption).lineLimit(compact ? 2 : 3).help(message)
                    .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                if player.bridgeFailed {
                    Button("Restart") { Task { await player.restartPlayer() } }.disabled(player.isRestarting)
                } else if player.canRetryPlayback {
                    Button("Retry", action: player.retryPlayback).accessibilityLabel("Retry playback at saved position")
                }
                if !player.bridgeFailed {
                    Button { player.errorMessage = nil } label: { Image(systemName: "xmark") }
                        .buttonStyle(.plain).accessibilityLabel("Dismiss error")
                }
            }.foregroundStyle(AuvraColors.warning).padding(compact ? 0 : 12)
                .background(compact ? Color.clear : AuvraColors.raised)
        }
    }
}
