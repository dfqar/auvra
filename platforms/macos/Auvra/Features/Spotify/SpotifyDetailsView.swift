import SwiftUI

/// Account and inspector queries have independent bounded state in the shared Store.
struct SpotifyProfileView: View {
    let player: PlayerStore
    private var connected: Bool { player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected }
    var body: some View {
        if connected {
            VStack(alignment: .leading, spacing: 10) {
                if let profile = player.spotifyDetail(.profile)?.collections.first {
                    HStack(spacing: 12) {
                        if let artwork = profile.artwork { ArtworkView(reference: artwork).frame(width: 40, height: 40).clipShape(Circle()) }
                        else { Image(systemName: "person.crop.circle").font(.largeTitle).foregroundStyle(.secondary).accessibilityHidden(true) }
                        VStack(alignment: .leading, spacing: 3) {
                            Text(profile.title).font(.headline)
                            Text(profile.id).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Spacer()
                        if let url = URL(string: profile.url) { Link("Profile", destination: url) }
                    }
                }
                SpotifyDetailStatus(player: player, kind: .profile)
            }.task(id: "\(connected)-\(player.spotifyDetailEpoch)") { if connected { player.inspectSpotify(.profile) } }
        }
    }
}
struct SpotifyDetailStatus: View {
    let player: PlayerStore
    let kind: RemoteCatalogKind
    var text = ""
    var body: some View {
        HStack(spacing: 10) {
            if player.isLoadingSpotifyDetail(kind, text: text) { ProgressView().controlSize(.mini).accessibilityLabel("Loading details") }
            else {
                if let message = player.spotifyDetail(kind, text: text)?.issue?.message { Text(message).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
                Button("Refresh details") { player.inspectSpotify(kind, text: text, refresh: true) }.font(.caption)
            }
        }.font(.caption)
    }
}
struct SpotifyCoverStatus: View {
    let player: PlayerStore
    let id: String
    var body: some View {
        if player.isLoadingSpotifyDetail(.playlistImages, text: id) {
            ProgressView("Refreshing cover…").controlSize(.small)
        } else if let page = player.spotifyDetail(.playlistImages, text: id) {
            if let issue = page.issue {
                HStack {
                    Text(issue.message).foregroundStyle(.secondary)
                    Button("Try cover again") { player.inspectSpotify(.playlistImages, text: id, refresh: true) }
                }.font(.caption)
            } else {
                Text(page.collections.first?.artwork == nil ? "No cover image" : "Cover refreshed").font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}
struct SpotifyPlaylistActions: View {
    let player: PlayerStore
    let playlist: RemoteCollection
    @Binding var editing: Bool
    @Binding var replacing: Bool
    private var access: PlaylistEditAccess { player.playlistEditAccess(playlist) }
    var body: some View {
        Menu("Playlist") {
            Button("Refresh cover") { player.inspectSpotify(.playlistImages, text: playlist.id, refresh: true) }
            if access.canEditItems { Button("Replace all items…") { replacing = true }.disabled(player.spotifyEditPending) }
            if access.canEditDetails {
                Button("Playlist settings…") { editing = true }
                Button("Upload JPEG cover…") { Task { await player.chooseSpotifyPlaylistCover(playlist.id) } }.disabled(player.spotifyEditPending)
            } else if access == .readOnly { Text("View only") }
            else if access == .unknown {
                Button("Refresh editing access") { player.inspectSpotify(.profile, refresh: true); player.inspectSpotify(.playlistInfo, text: playlist.id, refresh: true) }
            }
        }
    }
}
struct MediaItemInspector: View {
    let player: PlayerStore
    let item: MediaItem
    let navigation: AppNavigation

    var body: some View {
        if case .remote(let uri, _, _) = item.location, uri.hasPrefix("spotify:") {
            SpotifyItemInspector(player: player, item: item, navigation: navigation, title: "About This Track")
        } else {
            LocalItemInspector(item: item)
        }
    }
}

private struct LocalItemInspector: View {
    let item: MediaItem
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack {
                Text("About This Track").font(.headline).accessibilityAddTraits(.isHeader)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            HStack(alignment: .top, spacing: 16) {
                ArtworkView(reference: item.artwork).frame(width: 84, height: 84).clipShape(RoundedRectangle(cornerRadius: 8))
                VStack(alignment: .leading, spacing: 4) {
                    Text(item.title).font(.title2.weight(.semibold)).lineLimit(3).help(item.title)
                    Text(item.artist).foregroundStyle(.secondary).lineLimit(2).help(item.artist)
                }
            }
            TrackFacts(item: item)
            if case .file(let path) = item.location {
                VStack(alignment: .leading, spacing: 4) {
                    Text("File").font(.caption).foregroundStyle(.secondary)
                    Text(URL(fileURLWithPath: path).lastPathComponent).lineLimit(2).textSelection(.enabled)
                    Text(path).font(.caption).foregroundStyle(.secondary).lineLimit(3).textSelection(.enabled).help(path)
                }
            }
        }
        .padding(24).frame(width: 420, alignment: .leading)
        .background(AuvraColors.background).auvraAppearance()
    }
}

private struct TrackFacts: View {
    let item: MediaItem

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let album = item.album, !album.isEmpty {
                LabeledContent("Album") { Text(album).lineLimit(2).help(album) }
            }
            if item.durationMs > 0 {
                LabeledContent("Duration") { Text(formatTime(Double(item.durationMs) / 1000)).monospacedDigit() }
            }
        }.font(.callout)
    }
}

extension SpokenProgress {
    var label: String? {
        if fullyPlayed { return "Played" }
        return continuePosition.map { "At \(formatTime(Double($0) / 1000))" }
    }

    var continueLabel: String? {
        continuePosition.map { "Continue from \(formatTime(Double($0) / 1000))" }
    }
}

struct SpotifyItemInspector: View {
    let player: PlayerStore
    let item: MediaItem
    let navigation: AppNavigation
    var title: String? = nil
    @Environment(\.dismiss) private var dismiss
    @State private var requestedSpokenDetail: String?
    private var kind: RemoteCatalogKind {
        switch item.id.split(separator: ":").dropFirst().first { case "episode": .episode; case "chapter": .chapter; default: .track }
    }
    private var id: String { item.id.split(separator: ":").last.map(String.init) ?? "" }
    private var isSpokenDetail: Bool { kind == .episode || kind == .chapter }
    private var detailRequestKey: String { "\(item.id)-\(player.spotifyDetailEpoch)" }
    private var detail: RemotePage? { player.spotifyDetail(kind, text: id) }
    private var displayed: MediaItem { detail?.items.first ?? item }
    private func preview(_ text: String, limit: Int) -> String { text.count > limit ? String(text.prefix(limit)) + "…" : text }
    var body: some View {
        let progress = !isSpokenDetail || requestedSpokenDetail == detailRequestKey
            ? detail.flatMap { player.spotifySpokenProgress(for: displayed, queryId: $0.queryId) } : nil
        VStack(spacing: 0) {
        HStack {
            if let title { Text(title).font(.headline).accessibilityAddTraits(.isHeader) }
            Spacer()
            Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
        }.padding(.horizontal, 28).padding(.top, 24)
        ScrollView {
        VStack(alignment: .leading, spacing: 24) {
            HStack(alignment: .top, spacing: 24) {
                ArtworkView(reference: displayed.artwork).frame(width: 150, height: 150).clipShape(RoundedRectangle(cornerRadius: 10))
                VStack(alignment: .leading, spacing: 10) {
                    Text(preview(displayed.title, limit: 160)).font(.title2.weight(.semibold)).foregroundStyle(.primary).lineLimit(4).fixedSize(horizontal: false, vertical: true).help(displayed.title).accessibilityLabel(displayed.title)
                    Text(preview(displayed.artist, limit: 256)).foregroundStyle(.secondary).lineLimit(3).help(displayed.artist).accessibilityLabel(displayed.artist)
                    if title != nil { TrackFacts(item: displayed) }
                    if !displayed.capabilities.play { Text("Unavailable for playback.").font(.caption).foregroundStyle(.secondary) }
                    HStack {
                        Button("Play") { player.startSpotifyTrack(displayed) }.disabled(!displayed.capabilities.play)
                        Button("Add to Queue") { player.enqueueSpotifyTrack(displayed) }.disabled(!player.canEnqueueSpotifyTrack(displayed))
                    }
                    if let detail, let continuation = progress?.continueLabel {
                        Button(continuation) { player.continueSpotifySpoken(displayed, queryId: detail.queryId) }
                            .disabled(!displayed.capabilities.play || player.spotifyStartingItemID == displayed.id)
                    } else if progress?.fullyPlayed == true {
                        Label("Played", systemImage: "checkmark.circle").font(.caption).foregroundStyle(.secondary)
                    }
                    if displayed.capabilities.librarySave == true { SpotifyLibraryActions(player: player, uri: displayed.id) }
                    if let url = displayed.location?.externalURL { Link("Open in Spotify", destination: url) }
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
            let related = player.spotifyRelatedCollections(for: item)
            if !related.isEmpty {
                    VStack(spacing: 8) {
                        ForEach(related, id: \.uri) { collection in
                            Button {
                                dismiss(); navigation.openCollection(collection)
                            } label: {
                                HStack(spacing: 12) {
                                    ArtworkView(reference: collection.artwork).frame(width: 36, height: 36).clipShape(RoundedRectangle(cornerRadius: 6))
                                    VStack(alignment: .leading, spacing: 3) {
                                        Text(collection.title).lineLimit(2)
                                        Text(collection.entity.title).font(.caption).foregroundStyle(.secondary)
                                    }
                                    Spacer(); Image(systemName: "chevron.right").foregroundStyle(.secondary)
                                }.padding(10).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 8))
                            }.buttonStyle(.plain).accessibilityLabel("Open \(collection.entity.title): \(collection.title)")
                        }
                    }
            }
            SpotifyDetailStatus(player: player, kind: kind, text: id)
            SpotifyEditFeedback(player: player)
        }.padding(28)
        }
        }.frame(width: 580, height: 540).background(AuvraColors.background).auvraAppearance()
            .task(id: detailRequestKey) {
                guard !Task.isCancelled else { return }
                player.inspectSpotify(kind, text: id, refresh: isSpokenDetail)
                if isSpokenDetail { requestedSpokenDetail = detailRequestKey }
            }
            .onDisappear { if isSpokenDetail { requestedSpokenDetail = nil } }
    }
}
struct SpotifyLibraryActions: View {
    let player: PlayerStore
    let uri: String
    var body: some View {
        HStack {
            if let saved = player.spotifySaved[uri] {
                Button(saved ? "Remove from library" : "Save to library") { player.saveSpotifyURI(uri, save: !saved) }
            } else {
                Button("Check library status") { player.checkSpotifyURI(uri) }
                Button("Save to library") { player.saveSpotifyURI(uri, save: true) }
            }
        }.disabled(player.spotifyEditPending)
    }
}

struct SpotifyPlaylistReplacement: View {
    let player: PlayerStore
    let playlist: RemoteCollection
    @Environment(\.dismiss) private var dismiss
    @State private var input = ""
    @State private var allowsEmpty = false
    @State private var reviewing = false
    @State private var submitted = false
    private var draft: PlaylistReplacementDraft { PlaylistReplacementDraft(input) }
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(reviewing ? "Review replacement" : "Replace playlist items").font(.title2.weight(.semibold))
            Text(playlist.title).font(.headline).lineLimit(2)
            if reviewing {
                Text(draft.uris.isEmpty ? "This will remove every item and leave this playlist empty." : draft.uris.count == 1 ? "This item will replace all existing playlist items." : "These \(draft.uris.count) items will replace all existing playlist items, in this order.")
                Text("This cannot be undone in Auvra.").font(.caption).foregroundStyle(.secondary)
                ScrollView {
                    VStack(alignment: .leading, spacing: 8) {
                        ForEach(Array(draft.uris.enumerated()), id: \.offset) { index, uri in
                            HStack(alignment: .top) { Text("\(index + 1).").frame(width: 24, alignment: .trailing); Text(uri).textSelection(.enabled) }.font(.caption.monospaced())
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }.frame(height: 180)
            } else {
                Text("Paste one track or episode link or URI per line. Up to 100 items; order and duplicates are kept.").font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                TextEditor(text: $input).font(.body.monospaced()).frame(height: 180).accessibilityLabel("Replacement playlist items")
                    .padding(8).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 8))
                if let issue = draft.issue { Text(issue).font(.caption).foregroundStyle(AuvraColors.warning) }
                else if draft.uris.isEmpty { Toggle("Leave the playlist empty", isOn: $allowsEmpty) }
                else { Text(draft.uris.count == 1 ? "1 item" : "\(draft.uris.count) items").font(.caption).foregroundStyle(.secondary) }
            }
            if submitted { SpotifyEditFeedback(player: player) }
            HStack {
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction).disabled(player.spotifyEditPending)
                if reviewing { Button("Edit list") { reviewing = false }.disabled(player.spotifyEditPending) }
                Spacer()
                if reviewing {
                    Button("Replace all items", role: .destructive) {
                        submitted = true
                        player.editSpotify(.init(kind: "playlist_replace", uris: draft.uris, id: playlist.id))
                    }.disabled(player.spotifyEditPending || draft.issue != nil || draft.uris.isEmpty && !allowsEmpty)
                } else {
                    Button("Review replacement") { reviewing = true }.buttonStyle(.borderedProminent)
                        .disabled(draft.issue != nil || draft.uris.isEmpty && !allowsEmpty || player.spotifyEditPending)
                }
            }
        }.padding(28).frame(width: 540).background(AuvraColors.background).auvraAppearance()
            .onChange(of: player.spotifyEditPending) { old, pending in
                if submitted && old && !pending && player.spotifyEditIssue == nil && player.spotifyEditSuccess != nil { dismiss() }
            }
    }
}
