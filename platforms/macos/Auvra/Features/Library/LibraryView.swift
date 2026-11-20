import SwiftUI

struct LibraryView: View {
    let player: PlayerStore
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 28) {
                VStack(alignment: .leading, spacing: 8) {
                    Text("YOUR SPACE").font(.caption.weight(.semibold)).tracking(3).foregroundStyle(AuvraColors.accent)
                    Text("A little quieter.").font(.system(size: 42, weight: .medium, design: .serif))
                    Text("Your music, with room to breathe.").font(.title3).foregroundStyle(.secondary)
                }.padding(.top, 12)
                HStack(spacing: 14) {
                    Button { Task { await player.chooseLibraryFolder() } } label: { Label("Add Music Folder…", systemImage: "folder.badge.plus") }
                        .disabled(player.isChoosingFolder)
                    Button { player.scanLibrary() } label: { Label("Rescan", systemImage: "arrow.clockwise") }
                        .disabled(player.libraryState?.roots.isEmpty != false || player.isScanningLibrary)
                    if player.isScanningLibrary { Button("Cancel Scan", action: player.cancelLibraryScan) }
                    Spacer()
                    Button { Task { await player.chooseFile() } } label: { Label("Open File…", systemImage: "doc") }
                        .disabled(player.isChoosingFile)
                }
                if let state = player.libraryState {
                    if !state.roots.isEmpty { LibraryFoldersView(player: player, roots: state.roots) }
                    if let scan = state.scan {
                        HStack {
                            ProgressView().controlSize(.small)
                            Text("Scanning · \(scan.visited) checked · \(scan.imported) imported · \(scan.unchanged) unchanged · \(scan.failed) issues")
                                .font(.callout).foregroundStyle(.secondary)
                        }.accessibilityElement(children: .combine)
                    } else if let scan = state.lastScan {
                        Text("Last scan · \(scan.imported) imported · \(scan.unchanged) unchanged · \(scan.failed) issues")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    HStack {
                        Picker("Browse library", selection: Binding(get: { player.libraryQuery.albumId == nil ? player.libraryQuery.mode : .albums }, set: { player.browseLibrary(mode: $0) })) {
                            Text("Albums").tag(LibraryMode.albums); Text("Tracks").tag(LibraryMode.tracks)
                        }.pickerStyle(.segmented).labelsHidden().frame(width: 220)
                        Spacer()
                        Picker("Folders", selection: Binding(get: { player.libraryQuery.rootId ?? "" }, set: {
                            player.browseLibrary(rootId: $0.isEmpty ? nil : $0, clearRoot: $0.isEmpty)
                        })) {
                            Text("All folders").tag("")
                            ForEach(state.roots) { root in Text(root.name).tag(root.id) }
                        }.frame(maxWidth: 240)
                        Text("\(state.totalTracks) tracks · \(state.totalAlbums) albums").font(.caption).foregroundStyle(.secondary)
                    }
                }
                if let album = player.libraryAlbum {
                    HStack(spacing: 20) {
                        ArtworkView(reference: album.artwork).frame(width: 110, height: 110).clipShape(RoundedRectangle(cornerRadius: 14))
                        VStack(alignment: .leading, spacing: 8) {
                            Button { player.browseLibrary(mode: .albums) } label: { Label("All Albums", systemImage: "chevron.left") }.buttonStyle(.plain).font(.caption)
                            Text(album.title).font(.title2.weight(.medium))
                            Text(album.artist).foregroundStyle(.secondary)
                            Text("\(album.trackCount) \(album.trackCount == 1 ? "track" : "tracks")\(album.year.map { " · \($0)" } ?? "")").font(.caption).foregroundStyle(.secondary)
                            Button("Play Album", action: player.playLibraryAlbum).disabled(album.availableCount == 0 || player.isLoadingLibraryPage)
                        }
                        Spacer()
                    }
                }
                LibraryResultsView(player: player)
                if !player.localTracks.isEmpty {
                    Divider()
                    Text("OPENED THIS SESSION").font(.caption.weight(.semibold)).tracking(2).foregroundStyle(.secondary)
                    ForEach(Array(player.localTracks.enumerated()), id: \.element.id) { index, item in
                        TrackRow(item: item, index: index, active: player.current?.id == item.id) { player.play(item) }
                    }
                }
                Divider()
                HStack {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("DEMO COLLECTION").font(.caption.weight(.semibold)).tracking(2).foregroundStyle(.secondary)
                        Text("Quiet Hours").font(.title2)
                        DemoBadge()
                    }
                    Spacer()
                    Button("Play Demo") { if let first = player.demoTracks.first { player.play(first) } }
                }
                ForEach(Array(player.demoTracks.enumerated()), id: \.element.id) { index, item in
                    TrackRow(item: item, index: index, active: player.current?.id == item.id) { player.play(item) }
                        .contextMenu { Button("Play Next") { player.playNext(item) } }
                }
            }.padding(.horizontal, 38).padding(.vertical, 28)
                .frame(maxWidth: 1100, alignment: .leading).frame(maxWidth: .infinity)
        }.onAppear { player.browseLibrary(mode: .albums, text: "") }
    }
}

private struct LibraryFoldersView: View {
    let player: PlayerStore
    let roots: [LibraryRoot]
    private func status(_ root: LibraryRoot) -> String {
        switch root.status {
        case "ready": "Ready"
        case "scanning": "Scanning"
        case "partial": "Scan incomplete"
        case "cancelled": "Scan cancelled"
        default: "Needs access"
        }
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(roots) { root in
                HStack(spacing: 14) {
                    Image(systemName: root.authorized ? "folder" : "folder.badge.questionmark").foregroundStyle(AuvraColors.accent)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(root.name).font(.callout.weight(.medium))
                        Text(root.path).font(.caption2).foregroundStyle(.tertiary).lineLimit(1).help(root.path)
                        if let issue = root.issue { Text(issue).font(.caption).foregroundStyle(AuvraColors.warning).lineLimit(2) }
                    }
                    Spacer()
                    Text("\(root.trackCount) tracks · \(status(root))").font(.caption).foregroundStyle(.secondary)
                    if !root.authorized || root.status == "unavailable" {
                        Button("Reconnect…") { Task { await player.chooseLibraryFolder(replacing: root.id) } }.disabled(player.isChoosingFolder)
                    }
                    Menu {
                        Button("Rescan Folder") { player.scanLibrary(root.id) }.disabled(!root.authorized)
                        Button("Reconnect Folder…") { Task { await player.chooseLibraryFolder(replacing: root.id) } }
                        Button("Remove from Library", role: .destructive) { Task { await player.removeLibraryFolder(root.id) } }
                    } label: { Image(systemName: "ellipsis") }.menuStyle(.borderlessButton).frame(width: 24)
                        .accessibilityLabel("Manage \(root.name)").help("Manage folder. Removing it keeps your audio files.")
                }
            }
        }.padding(16).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 14))
    }
}

struct LibraryResultsView: View {
    let player: PlayerStore
    var body: some View {
        if player.libraryState?.ready != true {
            VStack(spacing: 12) {
                if let issue = player.libraryState?.issue { Text(issue).font(.callout).foregroundStyle(AuvraColors.warning) }
                else { ProgressView("Loading library…") }
                Button("Retry Library") { Task { await player.restoreLibrary() } }
            }.frame(maxWidth: .infinity).padding(30)
        } else if player.isLoadingLibraryPage {
            VStack(spacing: 12) {
                ProgressView("Loading music…")
                Button("Retry") { player.browseLibrary() }
            }.frame(maxWidth: .infinity).padding(30)
        } else if let page = player.libraryPage {
            if page.total == 0 {
                ContentUnavailableView(player.libraryQuery.text.isEmpty ? "Your library is quiet" : "No music found", systemImage: "music.note.list",
                                       description: Text(player.libraryQuery.text.isEmpty ? "Add a music folder, or rescan a folder already in your library." : "Try another track, artist or album."))
                    .frame(maxWidth: .infinity).padding(.vertical, 24)
            } else {
                if player.libraryQuery.mode == .albums {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 170, maximum: 230))], alignment: .leading, spacing: 24) {
                        ForEach(page.albums) { album in
                            Button { player.openLibraryAlbum(album) } label: {
                                VStack(alignment: .leading, spacing: 8) {
                                    ArtworkView(reference: album.artwork).clipShape(RoundedRectangle(cornerRadius: 14))
                                    Text(album.title).font(.callout.weight(.medium)).foregroundStyle(.primary).lineLimit(1)
                                    Text(album.artist).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                                    Text("\(album.trackCount) \(album.trackCount == 1 ? "track" : "tracks")\(album.availableCount == 0 ? " · Unavailable" : "")").font(.caption2).foregroundStyle(.tertiary)
                                }.contentShape(Rectangle())
                            }.buttonStyle(.plain).accessibilityLabel("Open album \(album.title) by \(album.artist), \(album.trackCount) \(album.trackCount == 1 ? "track" : "tracks")")
                        }
                    }
                } else {
                    LazyVStack(spacing: 2) {
                        ForEach(Array(page.tracks.enumerated()), id: \.element.id) { index, track in
                            VStack(alignment: .leading, spacing: 0) {
                                TrackRow(item: track.item, index: Int(page.offset) + index, active: player.current?.id == track.id) { player.playLibrary(track) }
                                    .disabled(!track.available).opacity(track.available ? 1 : 0.5)
                                    .contextMenu { Button("Play Next") { player.playNext(track.item) }.disabled(!track.available) }
                                if let issue = track.issue { Text(issue).font(.caption2).foregroundStyle(AuvraColors.warning).padding(.leading, 60).padding(.bottom, 8) }
                            }
                        }
                    }
                }
                HStack {
                    Text("\(page.offset + 1)–\(min(UInt64(page.offset) + UInt64(player.libraryQuery.limit), page.total)) of \(page.total)")
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    Button("Previous Page") { player.libraryPageBy(-1) }.disabled(page.offset == 0)
                    Button("Next Page") { player.libraryPageBy(1) }.disabled(UInt64(page.offset) + UInt64(player.libraryQuery.limit) >= page.total)
                }.padding(.top, 12)
            }
        }
    }
}
