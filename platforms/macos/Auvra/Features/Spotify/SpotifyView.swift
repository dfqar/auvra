import SwiftUI

struct SpotifyHomeView: View {
    let player: PlayerStore
    let navigation: AppNavigation
    @Environment(\.openSettings) private var openSettings
    @AppStorage("AuvraSettingsTab") private var settingsTab = "general"
    private var connected: Bool { player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected }
    private var discoveryArtist: RemoteCollection? { player.spotifyDetail(.homeTopArtists)?.collections.first { $0.entity == .artist } }

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 30) {
                hero
                HStack(spacing: 12) {
                    SpotifyLibraryShortcut(title: "Liked Songs", subtitle: "Your saved music", symbol: "heart.fill") {
                        navigation.show(.likedSongs)
                    }
                    if let stored = player.favoritePlaylist {
                        let favorite = player.collectionDetails(stored)
                        SpotifyLibraryShortcut(title: favorite.title, subtitle: "Pinned playlist", symbol: "pin.fill", artwork: favorite.artwork) {
                            navigation.openCollection(favorite)
                        }
                    } else {
                        SpotifyLibraryShortcut(title: "Your Library", subtitle: "Playlists and collections", symbol: "square.stack") {
                            navigation.show(.library)
                        }
                    }
                }
                if connected {
                    shelf(.homeRecent, subtitle: "Replay a song from your listening history.")
                    shelf(.homeTopTracks, subtitle: "Songs you return to most.")
                    if let artist = discoveryArtist {
                        shelf(.homeArtistAlbums, title: "More from \(artist.title)", text: artist.id,
                              subtitle: "Explore albums from an artist in your top picks.")
                    }
                    shelf(.homeRadio, subtitle: "Radio and mix playlists from catalog search.")
                    shelf(.homeTopArtists, subtitle: "Explore artists from your listening.")
                    shelf(.homePlaylists, subtitle: "Playlists saved to your account.")
                } else {
                    VStack(alignment: .leading, spacing: 12) {
                        Text(player.spotifyAccount.busy ? "Connecting your account…" : "Bring your music to Home.").font(.headline)
                        Text("Connect Spotify to see playlists and music based on your listening.").foregroundStyle(.secondary)
                        Button("Connect Spotify", action: openAccount).disabled(player.spotifyAccount.busy)
                    }.padding(20).frame(maxWidth: .infinity, alignment: .leading)
                        .background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 16))
                }
                discovery
            }.padding(32).frame(maxWidth: 1200, alignment: .leading).frame(maxWidth: .infinity, alignment: .leading)
        }
        .task(id: "\(connected)-\(player.spotifyDetailEpoch)-\(player.homePlaylistRevision)") {
            if connected { loadShelves() }
        }
        .task(id: "\(connected)-\(player.spotifyDetailEpoch)-\(discoveryArtist?.id ?? "")") {
            if connected, let artist = discoveryArtist { player.inspectSpotify(.homeArtistAlbums, text: artist.id) }
        }
    }

    private var hero: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("HOME").font(.caption.weight(.semibold)).tracking(3).foregroundStyle(AuvraColors.accent)
                Spacer()
                if connected {
                    Button { loadShelves(refresh: true) } label: { Image(systemName: "arrow.clockwise") }
                        .buttonStyle(.plain).accessibilityLabel("Refresh Home").help("Refresh Home")
                        .disabled(RemoteCatalogKind.homeKinds.contains { player.isLoadingSpotifyDetail($0) }
                                  || discoveryArtist.map { player.isLoadingSpotifyDetail(.homeArtistAlbums, text: $0.id) } == true)
                }
            }
            Text("What sounds good?").font(.system(size: 44, weight: .medium, design: .serif)).accessibilityAddTraits(.isHeader)
            Text("Find something new or return to a favorite.").foregroundStyle(.secondary)
            HStack(spacing: 10) {
                Button { navigation.focusSearch() } label: {
                    Label("Search music", systemImage: "magnifyingglass")
                }.buttonStyle(.borderedProminent)
                Button { Self.showNewAlbums(in: navigation) } label: {
                    Label("New albums", systemImage: "sparkles")
                }.buttonStyle(.bordered).help("Albums released in the past two weeks")
            }.padding(.top, 4)
        }.padding(24).frame(maxWidth: .infinity, alignment: .leading)
            .background {
                RoundedRectangle(cornerRadius: 20).fill(
                    LinearGradient(colors: [AuvraColors.accent.opacity(0.18), AuvraColors.card],
                                   startPoint: .topLeading, endPoint: .bottomTrailing)
                )
            }
    }

    static func showNewAlbums(in navigation: AppNavigation) {
        navigation.searchText = "tag:new"
        navigation.searchKind = .searchAlbums
        navigation.focusSearch()
    }

    private var discovery: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Explore by mood").font(.title2.weight(.semibold)).accessibilityAddTraits(.isHeader)
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 130), spacing: 10)], spacing: 10) {
                ForEach(Array([("Chill", "moon.stars"), ("Focus", "scope"), ("Jazz", "music.note"), ("Electronic", "waveform")].enumerated()), id: \.offset) { _, entry in
                    Button {
                        navigation.searchText = entry.0
                        navigation.searchKind = .searchPlaylists
                        navigation.focusSearch()
                    } label: {
                        Label(entry.0, systemImage: entry.1)
                            .font(.callout.weight(.medium)).frame(maxWidth: .infinity, alignment: .leading)
                            .padding(14).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 12))
                    }.buttonStyle(.plain)
                }
            }
        }
    }

    private func openAccount() { settingsTab = "account"; openSettings() }
    private func loadShelves(refresh: Bool = false) {
        for kind in RemoteCatalogKind.homeKinds { player.inspectSpotify(kind, refresh: refresh) }
        if refresh, let artist = discoveryArtist { player.inspectSpotify(.homeArtistAlbums, text: artist.id, refresh: true) }
    }

    private func shelf(_ kind: RemoteCatalogKind, title: String? = nil, text: String = "", subtitle: String) -> some View {
        let title = title ?? kind.title
        return VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.title2.weight(.semibold)).accessibilityAddTraits(.isHeader)
            Text(subtitle).font(.callout).foregroundStyle(.secondary)
            if let page = player.spotifyDetail(kind, text: text) {
                var seenTracks = Set<String>()
                let tracks = page.items.enumerated().filter { seenTracks.insert($0.element.id).inserted }
                if page.hasResults {
                    ScrollView(.horizontal) {
                        LazyHStack(alignment: .top, spacing: 14) {
                            ForEach(tracks.prefix(10), id: \.element.id) { index, item in
                                let recentPlay = kind == .homeRecent ? page.recentPlay(at: index) : nil
                                let playedDate = recentPlay?.playedDate
                                Button { player.startSpotifyTrack(item) } label: {
                                    tile(artwork: item.artwork, title: item.title, subtitle: item.artist, status: playedDate?.recentPlayCaption)
                                }.buttonStyle(.plain).disabled(!item.capabilities.play)
                                    .accessibilityLabel("Play \(item.title) by \(item.artist)")
                                    .accessibilityValue(playedDate?.formatted(date: .complete, time: .standard) ?? "")
                                    .contextMenu {
                                        if let context = recentPlay?.contextCollection {
                                            Button("Open playback context") {
                                                guard player.spotifyDetail(.homeRecent)?.queryId == page.queryId else { return }
                                                navigation.openCollection(context)
                                            }
                                        }
                                    }
                            }
                            ForEach(Array(page.collections.prefix(10).enumerated()), id: \.offset) { _, collection in
                                VStack(alignment: .leading, spacing: 0) {
                                    Button { navigation.openCollection(collection) } label: {
                                        tile(artwork: collection.artwork, title: collection.title,
                                             subtitle: collection.subtitle.flatMap { $0.isEmpty ? nil : $0 } ?? collection.entity.title)
                                    }.buttonStyle(.plain).accessibilityLabel("Open \(collection.entity.title) \(collection.title)")
                                    if collection.canPlayContext {
                                        Button { player.playSpotifyCollection(collection) } label: {
                                            Label("Play", systemImage: "play.fill").font(.caption.weight(.medium))
                                        }.buttonStyle(.borderless).padding(.horizontal, 10).padding(.bottom, 8)
                                            .accessibilityLabel("Play \(collection.title)")
                                    }
                                }.background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 14))
                            }
                        }.padding(.bottom, 4)
                    }
                } else if page.issue == nil {
                    Text("Nothing to show here yet.").font(.callout).foregroundStyle(.secondary)
                }
                if let issue = page.issue {
                    if page.hasOmittedResults {
                        Text(issue.message).font(.caption).foregroundStyle(.secondary)
                    } else {
                        HStack(spacing: 12) {
                            Text(issue.message).foregroundStyle(.secondary)
                            Spacer(minLength: 8)
                            Button("Try Again") { player.inspectSpotify(kind, text: text, refresh: true) }
                                .disabled(player.isLoadingSpotifyDetail(kind, text: text))
                        }.font(.callout)
                    }
                }
            } else {
                ProgressView().controlSize(.small).accessibilityLabel("Loading \(title)")
                    .frame(height: 32)
            }
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

    private func tile(artwork: String?, title: String, subtitle: String, status: String? = nil) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            ArtworkView(reference: artwork).frame(width: 136, height: 136).clipShape(RoundedRectangle(cornerRadius: 9))
            Text(title).font(.callout.weight(.semibold)).lineLimit(1)
            Text(subtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            if let status { Text(status).font(.caption2).foregroundStyle(.secondary).lineLimit(1) }
        }.padding(10).frame(width: 156, alignment: .leading)
            .background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 14))
    }
}

private extension Date {
    var recentPlayCaption: String { formatted(.dateTime.month(.abbreviated).day().hour().minute()) }
}

struct SpotifyLibraryView: View {
    let player: PlayerStore
    @Bindable var navigation: AppNavigation
    @State private var creating = false
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                HStack(alignment: .top) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Your Library").font(.system(size: 38, weight: .medium, design: .serif)).accessibilityAddTraits(.isHeader)
                        Text("Playlists, saved music and everything you keep.").foregroundStyle(.secondary)
                    }; Spacer()
                    if navigation.libraryKind == .playlists,
                       player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected {
                        Button { creating = true } label: { Label("New playlist", systemImage: "plus") }
                    }
                }
                HStack(spacing: 12) {
                    SpotifyLibraryShortcut(title: "Liked Songs", subtitle: "Your saved music", symbol: "heart.fill") {
                        navigation.show(.likedSongs)
                    }
                    if let stored = player.favoritePlaylist {
                        let favorite = player.collectionDetails(stored)
                        SpotifyLibraryShortcut(title: favorite.title, subtitle: "Pinned playlist", symbol: "pin.fill", artwork: favorite.artwork) {
                            navigation.openCollection(favorite)
                        }.accessibilityLabel("Open favorite playlist")
                    }
                }
                HStack {
                    Picker("Your library", selection: $navigation.libraryKind) {
                        ForEach(RemoteCatalogKind.libraryKinds) { kind in Text(kind.title).tag(kind) }
                    }.pickerStyle(.menu).frame(maxWidth: 240)
                    Spacer()
                    if [.topTracks, .topArtists].contains(navigation.libraryKind) {
                        Picker("Listening period", selection: $navigation.listeningPeriod) {
                            Text("4 weeks").tag("short_term"); Text("6 months").tag("medium_term"); Text("1 year").tag("long_term")
                        }.frame(width: 160)
                    }
                }
                SpotifyEditFeedback(player: player)
                if player.spotifyPlayback.status.phase != .ready,
                   player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected {
                    SpotifyPlaybackNotice(player: player)
                }
                SpotifyCatalogView(player: player, navigation: navigation, kind: navigation.libraryKind, text: [.topTracks, .topArtists].contains(navigation.libraryKind) ? navigation.listeningPeriod : "")
            }.padding(32).frame(maxWidth: 1100, alignment: .leading).frame(maxWidth: .infinity, alignment: .leading)
        }.sheet(isPresented: $creating) { SpotifyPlaylistEditor(player: player, playlist: nil) }
    }
}

struct SpotifyLikedSongsView: View {
    let player: PlayerStore
    let navigation: AppNavigation
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                Button { navigation.show(.library) } label: { Label("Your Library", systemImage: "chevron.left") }.buttonStyle(.plain)
                HStack(alignment: .bottom, spacing: 24) {
                    Image(systemName: "heart.fill").font(.system(size: 42, weight: .medium))
                        .foregroundStyle(AuvraColors.accent).frame(width: 120, height: 120)
                        .background(AuvraColors.accent.opacity(0.18), in: RoundedRectangle(cornerRadius: 14))
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 10) {
                        Text("Liked Songs").font(.system(size: 38, weight: .medium, design: .serif)).accessibilityAddTraits(.isHeader)
                        Text("All the songs you’ve saved.").foregroundStyle(.secondary)
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)
                SpotifyEditFeedback(player: player)
                SpotifyCatalogView(player: player, navigation: navigation, kind: .savedTracks)
            }.padding(32).frame(maxWidth: 1100, alignment: .leading).frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

private struct SpotifyLibraryShortcut: View {
    let title, subtitle, symbol: String
    var artwork: String? = nil
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                Group {
                    if let artwork { ArtworkView(reference: artwork) }
                    else {
                        Image(systemName: symbol).font(.headline).foregroundStyle(AuvraColors.accent)
                            .frame(maxWidth: .infinity, maxHeight: .infinity).background(AuvraColors.accent.opacity(0.14))
                    }
                }.frame(width: 44, height: 44).clipShape(RoundedRectangle(cornerRadius: 8)).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 4) {
                    Text(title).font(.headline).lineLimit(1)
                    Text(subtitle).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 4)
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary).accessibilityHidden(true)
            }.padding(12).frame(maxWidth: .infinity, alignment: .leading)
                .background(AuvraColors.cardEmphasis, in: RoundedRectangle(cornerRadius: 12))
        }.buttonStyle(.plain).accessibilityLabel(title)
    }
}

struct SpotifyPlaybackNotice: View {
    let player: PlayerStore
    @Environment(\.openSettings) private var openSettings
    @AppStorage("AuvraSettingsTab") private var settingsTab = "general"
    var body: some View {
        HStack(spacing: 12) {
            if player.spotifyPlayback.status.phase == .starting { ProgressView().controlSize(.small) }
            else { Image(systemName: "hifispeaker").foregroundStyle(.secondary) }
            Text(player.spotifyPlayback.status.phase == .starting ? "Preparing playback on this Mac…" : "Playback on this Mac needs attention.")
                .font(.callout).foregroundStyle(.secondary)
            Spacer()
            Button("Playback Settings") { settingsTab = "account"; openSettings() }
        }.padding(16).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 12))
    }
}

/// Pages are projected by PlayerStore; views never contact the provider.
struct SpotifyCatalogView: View {
    let player: PlayerStore
    let navigation: AppNavigation
    let kind: RemoteCatalogKind
    var text = ""
    var artworkFallback: String? = nil
    var parentCollection: RemoteCollection? = nil
    @State private var playlistItem: MediaItem?
    @State private var inspectedItem: MediaItem?
    @State private var requestedSpokenDetail: String?
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.openSettings) private var openSettings
    @AppStorage("AuvraSettingsTab") private var settingsTab = "general"
    private var connected: Bool { player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected }
    private var matchingQuery: Bool { player.spotifyQuery.kind == kind && (kind.usesCursor || player.spotifyQuery.text == text) }
    private var isSpokenDetail: Bool { kind == .episode || kind == .chapter }
    private var detailRequestKey: String { "\(kind.rawValue)-\(text)-\(player.spotifyDetailEpoch)" }
    private var page: RemotePage? { kind.isDetail ? player.spotifyDetail(kind, text: text) : player.spotifyPage }
    private var loading: Bool {
        if isSpokenDetail && requestedSpokenDetail != detailRequestKey { return true }
        if kind.isDetail { return page == nil || (kind != .artistAlbums && player.isLoadingSpotifyDetail(kind, text: text)) }
        return player.isLoadingSpotifyPage || !matchingQuery
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if !connected {
                VStack(alignment: .leading, spacing: 12) {
                    Text("Your Spotify library belongs here.").font(.headline)
                    Text(player.spotifyAccount.busy ? "Connecting your account…" : "Connect Spotify to browse your music, podcasts and audiobooks.").foregroundStyle(.secondary)
                    Button("Connect Spotify") { settingsTab = "account"; openSettings() }.disabled(player.spotifyAccount.busy)
                }.padding(24).frame(maxWidth: .infinity, alignment: .leading).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 16))
            } else if kind.isSearch && text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                Text("Search songs, albums, artists, playlists, podcasts and audiobooks.").foregroundStyle(.secondary)
            } else if loading {
                ProgressView("Loading…")
            } else if let page {
                if let issue = page.issue, !page.hasOmittedResults,
                   kind != .artistAlbums || player.spotifyNextArtistAlbumOffset(text) == nil {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(issue.message).foregroundStyle(.secondary).textSelection(.enabled)
                        HStack {
                            Button("Try Again") {
                                if kind.isDetail { player.inspectSpotify(kind, text: text, refresh: true) }
                                else if player.spotifyQuery.queryId == page.queryId { player.querySpotify(offset: page.offset) }
                            }
                            if page.needsCatalogAccountAccess {
                                Button("Account permissions…") { settingsTab = "account"; openSettings() }
                            }
                        }
                    }.padding(18).frame(maxWidth: .infinity, alignment: .leading).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 16))
                }
                if kind == .searchAll { mixedResults(page) }
                else { catalogResults(page) }
                if let issue = page.issue, page.hasOmittedResults {
                    Text(issue.message).font(.caption).foregroundStyle(.secondary)
                }
                if !page.hasResults && page.issue == nil {
                    if kind.isDetail { ContentUnavailableView("Content unavailable", systemImage: "link") }
                    else { ContentUnavailableView("Nothing here yet", systemImage: kind.isSearch ? "magnifyingglass" : "square.stack", description: Text(kind.isSearch ? "Try another name or content type." : "Saved items will appear here.")) }
                }
                if !kind.isDetail && page.hasPaginationData {
                    HStack {
                        Text(kind.usesCursor || kind == .searchAll ? "\(page.items.count + page.collections.count) shown" : page.total == 1 ? "1 item" : "\(page.total) items")
                            .font(.caption).foregroundStyle(.secondary)
                        Spacer()
                        Button(player.spotifyPreviousCursorIsFirst ? "First Page" : "Previous Page") { player.spotifyPageBy(-1) }
                            .disabled(page.offset == 0)
                        Button("Next Page") { player.spotifyPageBy(1) }
                            .disabled(kind.usesCursor ? !player.spotifyCanAdvanceCursor : (kind.isSearch && page.offset >= 1000) || UInt64(page.offset) + 10 >= page.total)
                    }
                } else if kind == .artistAlbums, let offset = player.spotifyNextArtistAlbumOffset(text) {
                    artistAlbumFooter(page, offset: offset)
                }
            }
        }.animation(AppMotion.feedback(reduceMotion), value: loading)
            .animation(AppMotion.navigation(reduceMotion), value: kind)
            .task(id: "\(connected)-\(kind.rawValue)-\(text)-\(playlistItem?.id ?? "")-\(kind.isDetail ? player.spotifyDetailEpoch : 0)") {
            guard !Task.isCancelled, playlistItem == nil, connected, !kind.isSearch || !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
            if isSpokenDetail {
                player.inspectSpotify(kind, text: text, refresh: requestedSpokenDetail != detailRequestKey)
                requestedSpokenDetail = detailRequestKey
                return
            }
            if kind.isSearch || kind.isDetail { do { try await Task.sleep(for: .milliseconds(300)) } catch { return } }
            guard !Task.isCancelled else { return }
            if kind.isDetail { player.inspectSpotify(kind, text: text) }
            else { player.presentSpotifyCatalog(kind: kind, text: text) }
        }.onDisappear { if isSpokenDetail { requestedSpokenDetail = nil } }
            .sheet(item: $playlistItem) { item in SpotifyPlaylistPicker(player: player, item: item) }
            .sheet(item: $inspectedItem) { item in SpotifyItemInspector(player: player, item: item, navigation: navigation) }
    }

    @ViewBuilder private func artistAlbumFooter(_ page: RemotePage, offset: UInt32) -> some View {
        if let issue = page.issue, issue.code != "track_unavailable", !player.isLoadingSpotifyDetail(kind, text: text) {
            HStack {
                Text(issue.message).font(.callout).foregroundStyle(.secondary)
                Spacer()
                Button("Retry loading releases") { player.loadMoreSpotifyArtistAlbums(text, at: offset) }
            }.padding(.vertical, 10)
        } else {
            ProgressView("Loading releases…").controlSize(.small).padding(.vertical, 10)
                .onGeometryChange(for: Bool.self) { geometry in
                    let scrollSpace = NamedCoordinateSpace.scrollView(axis: .vertical)
                    guard let bounds = geometry.bounds(of: scrollSpace), bounds.height > 0 else { return false }
                    return geometry.frame(in: scrollSpace).minY <= bounds.height + 120
                } action: { nearViewport in
                    if nearViewport { player.loadMoreSpotifyArtistAlbums(text, at: offset) }
                }
                .id(offset)
        }
    }

    @ViewBuilder private func catalogResults(_ page: RemotePage) -> some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 155, maximum: 220), spacing: 18)], alignment: .leading, spacing: 20) {
            ForEach(page.collections, id: \.uri) { collection in
                collectionButton(collection) {
                    VStack(alignment: .leading, spacing: 10) {
                        ArtworkView(reference: collection.artwork).aspectRatio(1, contentMode: .fit)
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                        Text(collection.title).font(.headline).lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
                        Text(collection.subtitle.flatMap { $0.isEmpty ? nil : $0 } ?? collection.entity.title)
                            .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }.padding(12).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 18))
                }
            }
        }
        LazyVStack(spacing: 4) {
            ForEach(Array(page.items.enumerated()), id: \.offset) { index, item in
                trackRow(item, index: index, page: page)
            }
        }
    }

    @ViewBuilder private func mixedResults(_ page: RemotePage) -> some View {
        let results = MixedSpotifyResults(page: page, query: text)
        if let artist = results.artist {
            VStack(alignment: .leading, spacing: 10) {
                Text("Top result").font(.title3.weight(.semibold)).accessibilityAddTraits(.isHeader)
                collectionRow(artist, featured: true)
            }
        }
        mixedSection("Music", hits: results.music, page: page)
        mixedSection("More results", hits: results.more, page: page)
    }

    @ViewBuilder private func mixedSection(_ title: String, hits: [MixedSpotifyResults.Hit], page: RemotePage) -> some View {
        if !hits.isEmpty {
            LazyVStack(alignment: .leading, spacing: 4) {
                Text(title).font(.title3.weight(.semibold)).accessibilityAddTraits(.isHeader).padding(.bottom, 6)
                ForEach(Array(hits.enumerated()), id: \.offset) { _, hit in
                    switch hit {
                    case .item(let index, let item): trackRow(item, index: index, page: page)
                    case .collection(let collection): collectionRow(collection)
                    }
                }
            }
        }
    }

    private func collectionRow(_ collection: RemoteCollection, featured: Bool = false) -> some View {
        collectionButton(collection) {
            HStack(spacing: 16) {
                if !featured {
                    Image(systemName: "square.stack").foregroundStyle(.secondary).frame(width: 26)
                        .accessibilityHidden(true)
                }
                ArtworkView(reference: collection.artwork)
                    .frame(width: featured ? 64 : 36, height: featured ? 64 : 36)
                    .clipShape(RoundedRectangle(cornerRadius: featured ? 10 : 4))
                VStack(alignment: .leading, spacing: 5) {
                    Text(collection.title).font(featured ? .title2.weight(.semibold) : .system(size: 14, weight: .medium)).lineLimit(1)
                    Text([collection.entity.title, collection.subtitle].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 16)
                Image(systemName: "chevron.right").font(.caption.weight(.medium)).foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 18).padding(.vertical, featured ? 16 : 14)
            .background(featured ? AuvraColors.cardEmphasis : AuvraColors.card, in: RoundedRectangle(cornerRadius: 12))
            .contentShape(Rectangle())
        }
    }

    private func collectionButton<Label: View>(_ collection: RemoteCollection, @ViewBuilder label: () -> Label) -> some View {
        Button { navigation.openCollection(collection) } label: { label() }
            .buttonStyle(.plain).accessibilityLabel("Open \(collection.entity.title) \(collection.title)")
            .contextMenu {
                if collection.canPlayContext { Button("Play") { player.playSpotifyCollection(collection) } }
                Button("Check library status") { player.checkSpotifyURI(collection.uri) }.disabled(player.spotifyEditPending)
                Button(player.spotifySaved[collection.uri] == true ? "Remove from library" : "Save to library") {
                    player.saveSpotifyURI(collection.uri, save: player.spotifySaved[collection.uri] != true)
                }.disabled(player.spotifyEditPending)
                if collection.entity == .playlist { Button("Pin as favorite") { player.pinPlaylist(collection) }.disabled(player.spotifyProfileID == nil) }
                if let url = URL(string: collection.url) { Link("Open in Spotify", destination: url) }
            }
    }

    private func trackRow(_ original: MediaItem, index: Int, page: RemotePage) -> some View {
        let item = player.spotifyCatalogItem(original, in: parentCollection)
        let position = page.position(at: index)
        let progress = player.spotifySpokenProgress(for: item, queryId: page.queryId)
        let isHistory = kind == .recent
        let recentPlay = isHistory ? page.recentPlay(at: index) : nil
        let playedDate = recentPlay?.playedDate
        let rowNumber = kind == .artistTopTracks || kind == .artistSelectedTracks ? index : Int(position ?? UInt32(clamping: Int(page.offset) + index))
        return TrackRow(item: item, index: rowNumber,
                        active: !isHistory && SpotifyPlaybackIdentity.matches(player.current?.id, item.id),
                        loading: !isHistory && player.spotifyStartingItemID == item.id, artworkFallback: artworkFallback, action: {
            if item.capabilities.play { player.playSpotifyCatalogItem(item, in: parentCollection, queryId: page.queryId, index: index) }
            else { inspectedItem = item }
        }, accessibilityAction: item.capabilities.play ? "Play" : "View details", status: playedDate?.recentPlayCaption ?? progress?.label)
            .opacity(item.capabilities.play ? 1 : 0.55)
            .accessibilityValue(playedDate?.formatted(date: .complete, time: .standard) ?? progress?.label ?? "")
            .contextMenu {
                if let context = recentPlay?.contextCollection {
                    Button("Open playback context") {
                        guard player.spotifyQuery.kind == .recent, player.spotifyQuery.queryId == page.queryId,
                              player.spotifyPage?.queryId == page.queryId else { return }
                        navigation.openCollection(context)
                    }
                }
                if let continuation = progress?.continueLabel {
                    Button(continuation) { player.continueSpotifySpoken(item, queryId: page.queryId) }
                        .disabled(!item.capabilities.play || player.spotifyStartingItemID == item.id)
                }
                Button("Details…") { inspectedItem = item }
                Button("Add to Queue") { player.enqueueSpotifyTrack(item) }.disabled(!player.canEnqueueSpotifyTrack(item))
                if item.capabilities.librarySave == true {
                    Button("Check library status") { player.checkSpotifyURI(item.id) }.disabled(player.spotifyEditPending)
                    Button(player.spotifySaved[item.id] == true ? "Remove from library" : "Save to library") {
                        player.saveSpotifyURI(item.id, save: player.spotifySaved[item.id] != true)
                    }.disabled(player.spotifyEditPending)
                    Button("Add to playlist…") { playlistItem = item }
                }
                if kind == .playlistItems, player.playlistEditAccess(id: text).canEditItems {
                    Button("Remove all copies from playlist") {
                        player.editSpotify(.init(kind: "playlist_remove", uris: [item.id], id: text))
                    }.disabled(player.spotifyEditPending)
                    if page.positionEdits == true, let position, position > 0 {
                        Button("Move up in playlist") {
                            player.editSpotify(.init(kind: "playlist_move", id: text, from: position, to: position - 1, queryId: page.queryId))
                        }.disabled(player.spotifyEditPending)
                    }
                    if page.positionEdits == true, let position, UInt64(position) + 1 < page.total, position < 99999 {
                        Button("Move down in playlist") {
                            player.editSpotify(.init(kind: "playlist_move", id: text, from: position, to: position + 2, queryId: page.queryId))
                        }.disabled(player.spotifyEditPending)
                    }
                }
                if let url = item.location?.externalURL { Link("Open in Spotify", destination: url) }
            }
    }
}

struct SpotifyDeviceMenu: View {
    let player: PlayerStore
    var body: some View {
        Menu {
            Button { player.useAuvraPlayback() } label: { Label("Play on this Mac · Auvra", systemImage: "desktopcomputer") }
            Divider()
            if player.spotifyAccount.phase == .signedIn {
                ForEach(player.spotifyState?.devices ?? []) { device in
                    Button { player.transferSpotify(device.id) } label: { Label(device.isActive ? "\(device.name) · Active" : device.name, systemImage: device.symbol) }.disabled(device.isRestricted)
                }
                Button("Refresh Devices", action: player.refreshSpotify)
            }
        } label: { Label(player.spotifyState?.playback?.device?.name ?? "This Mac · Auvra", systemImage: "hifispeaker") }.help("Choose where your music plays")
    }
}

struct SpotifyAccountView: View {
    @Bindable var player: PlayerStore
    var body: some View {
        Section("Spotify Premium") {
            SpotifyProfileView(player: player)
            if player.spotifyAccount.phase == .signedIn {
                HStack { Label("Connected", systemImage: "checkmark.circle"); Spacer(); Button("Disconnect", action: player.disconnectSpotify) }
                if let update = player.spotifyAccount.permissionUpdate {
                    HStack {
                        ProgressView(update == .requesting ? "Waiting for Spotify permissions…" : "Saving Spotify permissions…").controlSize(.small)
                        if update == .requesting { Button("Cancel", action: player.cancelSpotifyLogin) }
                    }
                } else { Button("Update Spotify Permissions", action: player.updateSpotifyPermissions) }
                Text("Update access to manage your library and playlists, see followed artists, history and personal tops, or read listening progress.")
                    .font(.caption).foregroundStyle(.secondary)
                spokenProgressPermission
            } else if player.spotifyAccount.busy {
                HStack { ProgressView().controlSize(.small); Text("Connecting your account…"); Button("Cancel", action: player.cancelSpotifyLogin) }
            } else if player.spotifyAccount.canRetrySavedLogin {
                SpotifySavedLoginRecovery(retryAvailableAt: player.spotifyAccount.retryAvailableAt, retry: player.retrySavedSpotifyLogin, signIn: player.connectSpotify)
                Text("Your saved login is kept. Retry uses its existing permissions without opening the browser.").font(.caption).foregroundStyle(.secondary)
            } else {
                TextField("Spotify app Client ID", text: $player.spotifyClientID).accessibilityLabel("Spotify Client ID")
                Button("Connect Spotify", action: player.connectSpotify)
                Text("Spotify asks for access to your library, playback controls and listening activity, including saved podcast and audiobook progress.")
                    .font(.caption).foregroundStyle(.secondary)
                Text("Add this Redirect URI in your Spotify app:").font(.caption).foregroundStyle(.secondary)
                Text(SpotifyOAuthClient.redirectURI).font(.caption.monospaced()).textSelection(.enabled)
                Link("Open Spotify Dashboard", destination: URL(string: "https://developer.spotify.com/dashboard")!)
            }
            if let issue = player.spotifyAccount.issue { Text(issue).foregroundStyle(AuvraColors.warning).textSelection(.enabled) }
            Text("When connected, Auvra keeps your login in Keychain. Your songs and playlists are available throughout the app.").font(.caption).foregroundStyle(.secondary)
            Link("Manage Spotify Permissions", destination: URL(string: "https://www.spotify.com/account/apps/")!)
        }
        Section("Playback on this Mac") {
            LabeledContent("Playback device") { SpotifyDeviceMenu(player: player) }
            SpotifyPlaybackSetup(player: player)
        }
    }
    @ViewBuilder private var spokenProgressPermission: some View {
        switch player.spotifyAccount.spokenProgressAccess {
        case .granted:
            LabeledContent("Listening progress", value: "Granted")
            Text("Continue appears when Spotify returns a saved position for an episode or chapter.")
                .font(.caption).foregroundStyle(.secondary)
        case .notGranted:
            LabeledContent("Listening progress", value: "Not granted")
            Text("Update permissions to read saved progress in podcasts and audiobooks.")
                .font(.caption).foregroundStyle(.secondary)
        case .unknown:
            LabeledContent("Listening progress", value: "Unknown")
            Text("Spotify did not report this permission for the saved login.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }
}

struct SpotifySavedLoginRecovery: View {
    let retryAvailableAt: Date?
    let retry, signIn: () -> Void
    @State private var passedDeadline: Date?
    var body: some View {
        let waiting = retryAvailableAt.map { $0 > Date() && passedDeadline != $0 } ?? false
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Button("Retry Saved Login", action: retry)
                Button("Sign In Again", action: signIn)
            }.disabled(waiting)
            if waiting { Text("Spotify asked Auvra to wait before retrying.").font(.caption).foregroundStyle(.secondary) }
        }.task(id: retryAvailableAt) {
            guard let deadline = retryAvailableAt else { return }
            let delay = deadline.timeIntervalSinceNow
            if delay > 0 { do { try await Task.sleep(for: .seconds(delay)) } catch { return } }
            guard !Task.isCancelled else { return }; passedDeadline = deadline
        }
    }
}

struct SpotifyPlaybackSetup: View {
    let player: PlayerStore
    @State private var key = ""
    private var phase: SoloistPhase { player.spotifyPlayback.status.phase }
    private var needsSetup: Bool { [.notConfigured, .failed, .expired].contains(phase) }
    private var deviceReady: Bool { phase == .ready && player.ownedSpotifyDevice != nil }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(deviceReady ? "Auvra playback is ready" : "Play Spotify directly in Auvra", systemImage: deviceReady ? "checkmark.circle" : "desktopcomputer").font(.headline)
            if needsSetup {
                Text("Download the Apple Silicon runtime, extract it, and select the soloist executable.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Link("Official Mac Runtime & Updates", destination: URL(string: "https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates")!)
                Button("Install Runtime…") { Task { await player.chooseSpotifyRuntime() } }
                    .disabled(player.isChoosingSpotifyRuntime || phase == .starting)
            }
            if player.spotifyPlayback.isInstalling { ProgressView("Verifying and installing…").controlSize(.small) }
            if let issue = player.spotifyPlayback.installIssue { Text(issue).font(.caption).foregroundStyle(AuvraColors.warning) }
            if let notice = player.spotifyPlayback.installNotice { Label(notice, systemImage: "checkmark.circle").font(.caption).foregroundStyle(AuvraColors.accent) }
            Divider()
            if needsSetup {
                Text("Create your personal Soloist key and save it here. Auvra keeps it in Keychain.").font(.callout).foregroundStyle(.secondary)
                Link("Create Soloist Key", destination: URL(string: "https://developer.spotify.com/dashboard/soloist")!)
                SecureField("Soloist API key", text: $key).accessibilityLabel("Soloist API key")
                HStack {
                    Button("Save Key & Start") { player.configureSpotifyPlayback(key: key); key = "" }.disabled(key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    if phase != .notConfigured { Button("Try Again", action: player.retrySpotifyPlayback) }
                }
            } else if phase == .starting { ProgressView("Starting playback engine…").controlSize(.small) }
            else if phase == .pairing {
                Text("Once only: in Spotify on the same network, select the device below. The connection is saved; afterward Spotify can stay closed.").foregroundStyle(.secondary)
                Text(player.spotifyPlayback.deviceName).font(.body.monospaced()).textSelection(.enabled)
                Button("Refresh Connection", action: player.refreshSpotify)
            } else if deviceReady {
                Text("Music plays through Auvra’s background engine, even with the Spotify app closed.").foregroundStyle(.secondary)
            } else {
                Text("Signed in on this Mac. Waiting for Spotify to list the Auvra playback device.").foregroundStyle(.secondary)
                Button("Refresh Connection", action: player.refreshSpotify)
            }
            if phase == .ready || phase == .pairing { Button("Reconnect Playback", action: player.retrySpotifyPlayback) }
            if let issue = player.spotifyPlayback.status.issue { Text(issue).font(.caption).foregroundStyle(AuvraColors.warning) }
        }.padding(16).frame(maxWidth: .infinity, alignment: .leading).background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 12))
            .disabled(player.spotifyPlayback.isForgetting || player.spotifyPlayback.isInstalling)
    }
}

struct SpotifyCollectionView: View {
    @State private var replacing = false
    let player: PlayerStore
    let navigation: AppNavigation
    @State private var editing = false
    var body: some View {
        ScrollView {
            if let original = navigation.selectedCollection {
                let collection = player.collectionDetails(original)
                let isArtist = collection.entity == .artist
                LazyVStack(alignment: .leading, spacing: 28) {
                    Button(action: navigation.leaveCollection) { Label("Back", systemImage: "chevron.left") }.buttonStyle(.plain)
                    if isArtist {
                        ArtistAmbientBanner(artworkReference: collection.artwork) {
                            collectionHeader(collection)
                        }
                    } else { collectionHeader(collection) }
                    if let description = collection.description, !description.isEmpty {
                        // Spotify descriptions may contain HTML. Render as inert text, never a web view.
                        Text(description.replacingOccurrences(of: "<[^>]+>", with: "", options: .regularExpression)).font(.callout).foregroundStyle(.secondary)
                    }
                    SpotifyDetailStatus(player: player, kind: collection.detailKind, text: collection.id)
                    if collection.entity == .playlist { SpotifyCoverStatus(player: player, id: collection.id) }
                    SpotifyEditFeedback(player: player)
                    if isArtist { artistTracks(collection) }
                    Text(isArtist ? "Discography" : collection.entity == .show ? "Episodes" : collection.entity == .audiobook ? "Chapters" : "Tracks")
                        .font(.title2.weight(.semibold)).accessibilityAddTraits(.isHeader)
                    SpotifyCatalogView(player: player, navigation: navigation, kind: collection.childrenKind, text: collection.id, artworkFallback: collection.artwork, parentCollection: collection).id(collection.id)
                }.padding(32).frame(maxWidth: 1100, alignment: .leading).frame(maxWidth: .infinity, alignment: .leading)
                    .task(id: "\(collection.uri)-\(player.spotifyDetailEpoch)-\(player.spotifyState?.connection.rawValue ?? "")") {
                        if player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected { player.inspectSpotify(collection.detailKind, text: collection.id) }
                    }
            }
        }.sheet(isPresented: $editing) { SpotifyPlaylistEditor(player: player, playlist: navigation.selectedCollection.map(player.collectionDetails)) }
            .sheet(isPresented: $replacing) { if let playlist = navigation.selectedCollection.map(player.collectionDetails) { SpotifyPlaylistReplacement(player: player, playlist: playlist) } }
    }

    private func collectionHeader(_ collection: RemoteCollection) -> some View {
        let isArtist = collection.entity == .artist
        return HStack(alignment: .bottom, spacing: 28) {
            ArtworkView(reference: collection.artwork).frame(width: isArtist ? 212 : 180, height: isArtist ? 212 : 180)
                .clipShape(RoundedRectangle(cornerRadius: isArtist ? 18 : 12))
            VStack(alignment: .leading, spacing: 12) {
                Text(collection.entity.title.uppercased()).font(.caption.weight(.semibold)).tracking(3)
                    .foregroundStyle(isArtist ? .white.opacity(0.80) : AuvraColors.accent)
                Text(collection.title).font(.system(size: isArtist ? 46 : 36, weight: .medium, design: .serif)).lineLimit(3)
                    .accessibilityAddTraits(.isHeader)
                if let subtitle = collection.subtitle, !subtitle.isEmpty {
                    Text(subtitle).foregroundColor(isArtist ? .white.opacity(0.82) : .secondary)
                }
                HStack {
                    if collection.canPlayContext { Button { player.playSpotifyCollection(collection) } label: { Label("Play", systemImage: "play.fill") }.buttonStyle(.borderedProminent) }
                    Menu("Library") {
                        Button(player.spotifySaved[collection.uri] == true ? "Remove from library" : "Save to library") { player.saveSpotifyURI(collection.uri, save: player.spotifySaved[collection.uri] != true) }
                        Button("Check library status") { player.checkSpotifyURI(collection.uri) }
                        if collection.entity == .playlist {
                            let pinned = player.favoritePlaylistID == collection.id
                            Button(pinned ? "Unpin favorite" : "Pin as favorite") { player.pinPlaylist(pinned ? nil : collection) }
                                .disabled(player.spotifyProfileID == nil)
                        }
                    }.disabled(player.spotifyEditPending)
                    if collection.entity == .playlist {
                        SpotifyPlaylistActions(player: player, playlist: collection, editing: $editing, replacing: $replacing)
                    }
                    if let url = URL(string: collection.url) {
                        if isArtist { Link("Open in Spotify", destination: url).tint(.white.opacity(0.90)) }
                        else { Link("Open in Spotify", destination: url) }
                    }
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func artistTracks(_ artist: RemoteCollection) -> some View {
        let top = player.spotifyDetail(.artistTopTracks, text: artist.id)
        let selected = top.map { $0.issue?.code == "unsupported_capability" || ($0.issue == nil && !$0.hasResults) } == true
        return VStack(alignment: .leading, spacing: 12) {
            Text(selected ? "Selected tracks" : "Top tracks").font(.title2.weight(.semibold)).accessibilityAddTraits(.isHeader)
            SpotifyCatalogView(player: player, navigation: navigation, kind: selected ? .artistSelectedTracks : .artistTopTracks,
                               text: artist.id, artworkFallback: artist.artwork, parentCollection: artist)
        }
    }
}

struct SpotifyEditFeedback: View {
    let player: PlayerStore
    var body: some View {
        if player.spotifyEditPending { ProgressView("Updating your library…").controlSize(.small) }
        else if let issue = player.spotifyEditIssue { Text(issue).font(.callout).foregroundStyle(AuvraColors.warning).textSelection(.enabled) }
        else if let message = player.spotifyEditSuccess { Label(message, systemImage: "checkmark.circle").font(.caption).foregroundStyle(AuvraColors.accent) }
    }
}

struct SpotifyPlaylistEditor: View {
    let player: PlayerStore
    let playlist: RemoteCollection?
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var description = ""
    @State private var isPublic = false
    @State private var isCollaborative = false
    @State private var submitted = false
    @State private var original: RemoteCollection?
    @State private var originalEpoch: UInt64?
    private var edit: RemoteEdit? {
        guard originalEpoch == player.spotifyDetailEpoch else { return nil }
        if let playlist {
            guard let original, original.id == playlist.id else { return nil }
            return .playlistDetailsChanges(original, name: name, description: description, published: isPublic, collaborating: isCollaborative)
        }
        return .createPlaylist(name: name, description: description, published: isPublic, collaborating: isCollaborative)
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            Text(playlist == nil ? "New playlist" : "Edit playlist").font(.title2.weight(.semibold))
            TextField("Playlist name", text: $name).textFieldStyle(.roundedBorder)
            TextField("Description", text: $description, axis: .vertical).lineLimit(3...5).textFieldStyle(.roundedBorder)
            VStack(alignment: .leading, spacing: 10) {
                Toggle("Publish on my profile", isOn: $isPublic).disabled(isCollaborative && !isPublic)
                    .help("Turn off collaboration before publishing on your profile.")
                Toggle("Collaborative playlist", isOn: $isCollaborative).disabled(isPublic && !isCollaborative)
                    .help("Turn off profile publishing before enabling collaboration.")
                Text("Collaboration lets other users edit this playlist. Collaborative playlists stay off your profile.").font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                if isPublic && isCollaborative { Text("Choose profile publishing or collaboration before saving.").font(.caption).foregroundStyle(AuvraColors.warning) }
            }
            Text("Profile publishing does not control who can open this playlist. Use Spotify to make it private.").font(.caption).foregroundStyle(.secondary)
            Text(playlist == nil ? "The playlist starts empty. Add songs from their context menu." : "Spotify permits changes only when your account has editing rights.").font(.caption).foregroundStyle(.secondary)
            if submitted { SpotifyEditFeedback(player: player) }
            HStack {
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction).disabled(player.spotifyEditPending)
                Spacer()
                Button(playlist == nil ? "Create playlist" : "Save changes") {
                    guard let edit else { return }
                    submitted = true
                    player.editSpotify(edit)
                }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                    .disabled(edit == nil || player.spotifyEditPending)
            }
        }.padding(28).frame(width: 460).background(AuvraColors.background).auvraAppearance()
            .onAppear {
                guard originalEpoch == nil else { return }
                original = playlist; originalEpoch = player.spotifyDetailEpoch
                name = playlist?.title ?? ""; description = playlist?.description ?? ""; isPublic = playlist?.isPublic ?? false; isCollaborative = playlist?.collaborative ?? false
            }
            .onChange(of: player.spotifyDetailEpoch) { _, _ in dismiss() }
            .onChange(of: playlist?.id) { _, _ in dismiss() }
            .onChange(of: player.spotifyEditPending) { old, pending in if submitted && old && !pending && player.spotifyEditIssue == nil && player.spotifyEditSuccess != nil { dismiss() } }
    }
}

struct SpotifyPlaylistPicker: View {
    let player: PlayerStore
    let item: MediaItem
    @Environment(\.dismiss) private var dismiss
    @State private var submitted = false
    @State private var previousQuery: RemoteQuery?
    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack { Text("Add to playlist").font(.title2.weight(.semibold)); Spacer(); Button("Done") { dismiss() } }
            Text(item.title).foregroundStyle(.secondary)
            if submitted { SpotifyEditFeedback(player: player) }
            if player.spotifyQuery.kind != .playlists || player.isLoadingSpotifyPage { ProgressView("Loading playlists…") }
            else if let page = player.spotifyPage {
                if let issue = page.issue {
                    Text(issue.message).foregroundStyle(.secondary)
                    if !page.hasPaginationData {
                        Button("Try Again") {
                            guard player.spotifyQuery.queryId == page.queryId else { return }
                            player.querySpotify(offset: page.offset)
                        }
                    }
                }
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(page.collections) { playlist in
                            Button {
                                submitted = true
                                player.editSpotify(.init(kind: "playlist_add", uris: [item.id], id: playlist.id))
                            } label: {
                                HStack { ArtworkView(reference: playlist.artwork).frame(width: 44, height: 44); Text(playlist.title); Spacer(); Image(systemName: player.playlistEditAccess(playlist).canEditItems ? "plus" : "lock") }.padding(10)
                            }.buttonStyle(.plain).disabled(player.spotifyEditPending || !player.playlistEditAccess(playlist).canEditItems)
                        }
                    }
                }
                if page.hasPaginationData {
                    HStack { Button("Previous Page") { player.spotifyPageBy(-1) }.disabled(page.offset == 0); Spacer(); Button("Next Page") { player.spotifyPageBy(1) }.disabled(UInt64(page.offset) + 10 >= page.total) }
                }
            }
        }.padding(28).frame(width: 480, height: 480).background(AuvraColors.background).auvraAppearance()
            .task { if previousQuery == nil { previousQuery = player.spotifyQuery }; player.inspectSpotify(.profile); player.querySpotify(kind: .playlists, text: "") }
            .onDisappear { if let query = previousQuery { player.querySpotify(kind: query.kind, text: query.text, offset: query.offset) } }
            .onChange(of: player.spotifyEditPending) { old, pending in if submitted && old && !pending && player.spotifyEditIssue == nil && player.spotifyEditSuccess != nil { dismiss() } }
    }
}

extension RemotePage {
    var hasPaginationData: Bool { issue == nil || hasResults }
    var needsCatalogAccountAccess: Bool { issue?.code == "permission_lost" || issue?.code == "authentication_required" }
}
