import SwiftUI

struct QueueView: View {
    let player: PlayerStore
    let navigation: AppNavigation
    var dismissPanel: (() -> Void)? = nil
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var isPanel: Bool { dismissPanel != nil }
    private var inset: CGFloat { isPanel ? 16 : 28 }
    private var localSession: SessionState? {
        guard player.canEditQueue, let session = player.snapshot?.session, session.enabled else { return nil }
        return session
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Queue")
                    .font(isPanel ? .headline : .system(size: 28, weight: .medium, design: .serif))
                    .accessibilityAddTraits(.isHeader)
                Spacer()
                if player.canEditQueue && player.canNavigateQueue {
                    Button("Clear Queue", action: player.clearQueue).controlSize(.small)
                }
                if let dismissPanel {
                    Button(action: dismissPanel) { Image(systemName: "xmark") }
                        .buttonStyle(.plain).foregroundStyle(.secondary).padding(5)
                        .accessibilityLabel("Close queue").help("Close queue")
                }
            }.padding(inset)

            Divider()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    status.font(.caption)
                    if let current = player.current { currentSection(current) }
                    if let queue = player.snapshot?.queue, !queue.upcoming.isEmpty { upcomingSection(queue) }
                    else if player.snapshot != nil && localSession?.ready != false { emptyState }
                    if let deviceQueue = player.snapshot?.queue.deviceQueue, !deviceQueue.previous.isEmpty {
                        sectionLabel("Previously played").padding(.top, 6)
                        ForEach(Array(deviceQueue.previous.enumerated()), id: \.offset) { index, item in
                            QueueTrackRow(item: item, provenance: Self.sourceLabel(
                                in: deviceQueue.previousSources, rowCount: deviceQueue.previous.count, at: index
                            ))
                        }
                    }
                }
                .padding(inset)
                .animation(AppMotion.navigation(reduceMotion), value: player.snapshot?.queue.upcoming.map(\.id))
                .animation(AppMotion.navigation(reduceMotion), value: player.snapshot?.queue.deviceQueue?.previous.map(\.id))
                .animation(AppMotion.navigation(reduceMotion), value: player.current?.id)
            }
        }.frame(maxWidth: 720).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    }

    @ViewBuilder private var status: some View {
        if player.snapshot == nil && !player.bridgeFailed {
            ProgressView("Preparing queue…").controlSize(.small)
        }
        if let session = localSession {
            if let issue = session.issue {
                VStack(alignment: .leading, spacing: 6) {
                    Text(issue.message).foregroundStyle(AuvraColors.warning)
                    Button("Try Again") { Task { await player.restoreQueue() } }
                }
            } else if !session.ready {
                ProgressView("Restoring queue…").controlSize(.small)
            } else if session.restored && player.canNavigateQueue {
                Text("Queue restored. Press Play when you’re ready.").foregroundStyle(.secondary)
            } else if session.saving {
                Text("Saving queue…").foregroundStyle(.secondary)
            }
        }
        if !player.canEditQueue && player.current != nil {
            Text("This queue is managed on the playback device.").foregroundStyle(.secondary)
        }
        if player.snapshot?.enhancement == .recommendations {
            Label("Recommendations enabled", systemImage: "sparkles").foregroundStyle(.secondary)
        }
    }

    private func currentSection(_ current: MediaItem) -> some View {
        let pauses = player.playbackToggleShowsPause
        return VStack(alignment: .leading, spacing: 6) {
            sectionLabel(player.isPlaying ? "Now playing" : "Selected track")
            Button {
                guard player.current?.id == current.id else { return }
                player.togglePlayback()
            } label: {
                QueueTrackRow(item: current, highlighted: true, symbol: pauses ? "pause.fill" : "play.fill")
            }
            .buttonStyle(.plain).disabled(!player.canTogglePlayback)
            .accessibilityLabel("\(pauses ? "Pause" : "Play") \(current.title) by \(current.artist)")
            if let issue = player.queueIssue(current.id) {
                Text(issue).font(.caption).foregroundStyle(AuvraColors.warning)
                HStack {
                    Button("Manage Folders") { navigate(to: .localLibrary) }
                    Button("Open File…") {
                        dismissPanel?()
                        Task { await player.chooseFile() }
                    }
                    Button("Check Access") { Task { await player.restoreQueue() } }
                }.controlSize(.small)
            }
        }
    }

    @ViewBuilder private func upcomingSection(_ queue: QueueState) -> some View {
        HStack {
            sectionLabel("Up next")
            Spacer()
            Text(queue.deviceQueue != nil ? "\(queue.upcoming.count) shown" : queue.upcoming.count == 1 ? "1 track" : "\(queue.upcoming.count) tracks")
                .font(.caption).foregroundStyle(.secondary)
        }.padding(.top, 6)
        ForEach(Array(queue.upcoming.enumerated()), id: \.offset) { index, item in
            VStack(alignment: .leading, spacing: 4) {
                if index == 0 && queue.playNextCount > 0 { sectionLabel("Play next") }
                if index == queue.playNextCount && queue.forwardCount > 0 { sectionLabel("Returning to") }
                if player.canEditQueue && index == queue.playNextCount + queue.forwardCount { sectionLabel("From your collection") }
                if player.canEditQueue {
                    HStack(spacing: 8) {
                        Button {
                            guard player.canEditQueue, player.snapshot?.queue.revision == queue.revision else { return }
                            player.jumpQueue(index)
                        } label: {
                            QueueTrackRow(item: item)
                        }.buttonStyle(.plain)
                            .disabled(player.queueIssue(item.id) != nil)
                            .accessibilityLabel("\(player.isPlaying ? "Play" : "Select") \(item.title) by \(item.artist)")
                        Button {
                            guard player.canEditQueue, player.snapshot?.queue.revision == queue.revision else { return }
                            player.removeQueueItem(index)
                        } label: {
                            Image(systemName: "minus.circle").frame(width: 22, height: 26)
                        }.buttonStyle(.plain).foregroundStyle(.secondary)
                            .accessibilityLabel("Remove \(item.title) from queue").help("Remove from queue")
                    }
                } else {
                    QueueTrackRow(item: item, provenance: Self.sourceLabel(
                        in: queue.deviceQueue?.upcomingSources, rowCount: queue.upcoming.count, at: index
                    ))
                }
                if let issue = player.queueIssue(item.id) { Text(issue).font(.caption).foregroundStyle(AuvraColors.warning) }
            }
        }
    }

    private var emptyState: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(player.current == nil ? "Queue is empty" : "Nothing up next").font(.headline)
            Group {
                if player.current == nil || player.canEditQueue { Text("Add a track from your library.") }
                else if player.snapshot?.queue.deviceQueue != nil { Text("There are no upcoming tracks on this device.") }
                else { Text("The playback device has not reported upcoming tracks.") }
            }.font(.callout).foregroundStyle(.secondary)
            Button("Browse Library") { navigate(to: .library) }.controlSize(.small)
        }.padding(12).frame(maxWidth: .infinity, alignment: .leading)
            .background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 10))
    }

    static func sourceLabel(in sources: [QueueEntrySource?]?, rowCount: Int, at index: Int) -> String? {
        guard let sources, sources.count == rowCount, sources.indices.contains(index) else { return nil }
        switch sources[index] {
        case .autoplay: return "Autoplay"
        case .queue: return "Queued"
        default: return nil
        }
    }

    private func sectionLabel(_ title: String) -> some View {
        Text(title).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            .accessibilityAddTraits(.isHeader)
    }

    private func navigate(to page: AppPage) {
        dismissPanel?()
        navigation.show(page)
    }
}

private struct QueueTrackRow: View {
    let item: MediaItem
    var highlighted = false
    var symbol: String? = nil
    var provenance: String? = nil

    var body: some View {
        HStack(spacing: 10) {
            ArtworkView(reference: item.artwork)
                .frame(width: 34, height: 34).clipShape(RoundedRectangle(cornerRadius: 5))
            VStack(alignment: .leading, spacing: 3) {
                Text(item.title).font(.callout.weight(.medium)).lineLimit(1)
                Text(item.artist).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 4)
            if let symbol { Image(systemName: symbol).foregroundStyle(AuvraColors.accent).accessibilityHidden(true) }
            if let provenance {
                Text(provenance).font(.caption2).foregroundStyle(.secondary)
                    .padding(.horizontal, 7).padding(.vertical, 3).background(AuvraColors.subtle, in: Capsule())
            }
        }.padding(6)
            .background(highlighted ? AuvraColors.selection : .clear, in: RoundedRectangle(cornerRadius: 8))
            .contentShape(Rectangle())
            .accessibilityElement(children: .combine)
    }
}
