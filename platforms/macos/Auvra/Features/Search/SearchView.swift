import SwiftUI

enum SpotifySearchInput: Equatable {
    case keywords(String)
    case reference(RemoteCatalogKind, String)
    case userProfile(String)
    case unsupportedLink

    init(_ input: String) {
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        let lower = text.lowercased()
        let uri = lower.hasPrefix("spotify:")
        let bareLink = lower.hasPrefix("open.spotify.com/")
        guard uri || bareLink || lower.hasPrefix("http://") || lower.hasPrefix("https://") else {
            self = .keywords(RemoteQuery.boundedText(text))
            return
        }
        guard text.utf8.count <= 2048 else { self = .unsupportedLink; return }
        let normalized: String?
        if uri {
            normalized = "spotify:" + text.dropFirst("spotify:".count)
        } else {
            guard var url = URLComponents(string: bareLink ? "https://" + text : text),
                  url.percentEncodedHost?.lowercased() == "open.spotify.com",
                  url.user == nil, url.password == nil, url.port == nil else {
                self = .unsupportedLink
                return
            }
            url.scheme = "https"
            url.host = "open.spotify.com"
            url.query = nil
            url.fragment = nil
            normalized = url.string
        }
        if let normalized, let id = Self.userProfileID(normalized) {
            self = .userProfile(id)
            return
        }
        guard let normalized, let reference = SpotifyContentReference(normalized) else {
            self = .unsupportedLink
            return
        }
        self = .reference(reference.kind, reference.id)
    }

    private static func userProfileID(_ reference: String) -> String? {
        let prefix = "spotify:user:"
        let id: Substring
        if reference.hasPrefix(prefix) {
            id = reference.dropFirst(prefix.count)
        } else {
            guard let url = URLComponents(string: reference), url.percentEncodedPath.hasPrefix("/") else { return nil }
            var path = url.percentEncodedPath.dropFirst().split(separator: "/", omittingEmptySubsequences: false)
            if path.first?.hasPrefix("intl-") == true { path.removeFirst() }
            if path.last == "" { path.removeLast() }
            guard path.count == 2, path[0] == "user" else { return nil }
            id = path[1]
        }
        guard (1...128).contains(id.utf8.count), id != ".", id != "..",
              id.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || "-._~".utf8.contains($0) }) else { return nil }
        return String(id)
    }
}

struct MixedSpotifyResults {
    enum Hit {
        case item(Int, MediaItem)
        case collection(RemoteCollection)

        var title: String {
            switch self {
            case .item(_, let item): item.title
            case .collection(let collection): collection.title
            }
        }

        var creators: [String] {
            switch self {
            case .item(_, let item): return item.artists
            case .collection(let collection):
                guard let subtitle = collection.subtitle else { return [] }
                return [subtitle] + subtitle.components(separatedBy: ", ")
            }
        }
    }
    let artist: RemoteCollection?
    let music: [Hit]
    let more: [Hit]

    init(page: RemotePage, query: String) {
        let key = Self.normalized(query)
        let artists = page.collections.filter { $0.entity == .artist }
        let exact = artists.first { !key.isEmpty && Self.normalized($0.title) == key }
        let close = artists.filter { Self.oneEditAway(key, Self.normalized($0.title)) }
        let topArtist = exact ?? (close.count == 1 ? close.first : nil)
        artist = topArtist

        let songs = page.items.enumerated().compactMap { index, item in
            item.id.hasPrefix("spotify:track:") ? Hit.item(index, item) : nil
        }
        let albums = page.collections.filter { $0.entity == .album }.map(Hit.collection)
        let artistKey = topArtist.map { Self.normalized($0.title) }
        func rank(_ hit: Hit) -> Int {
            if let artistKey, hit.creators.contains(where: { Self.normalized($0) == artistKey }) { return 0 }
            return Self.normalized(hit.title) == key ? 1 : 2
        }
        let rankedSongs = songs.map { (hit: $0, rank: rank($0)) }
        let rankedAlbums = albums.map { (hit: $0, rank: rank($0)) }
        var interleaved: [Hit] = []
        for priority in 0...2 {
            let tracks = rankedSongs.filter { $0.rank == priority }
            let releases = rankedAlbums.filter { $0.rank == priority }
            for index in 0..<max(tracks.count, releases.count) {
                if tracks.indices.contains(index) { interleaved.append(tracks[index].hit) }
                if releases.indices.contains(index) { interleaved.append(releases[index].hit) }
            }
        }
        music = interleaved
        more = page.collections.filter { $0.entity != .album && $0.uri != topArtist?.uri }.map(Hit.collection)
            + page.items.enumerated().compactMap { index, item in
                item.id.hasPrefix("spotify:track:") ? nil : Hit.item(index, item)
            }
    }

    private static func normalized(_ text: String) -> String {
        text.folding(options: [.caseInsensitive, .diacriticInsensitive, .widthInsensitive], locale: Locale(identifier: "en_US_POSIX"))
            .split { !$0.isLetter && !$0.isNumber }.joined(separator: " ")
    }

    private static func oneEditAway(_ first: String, _ second: String) -> Bool {
        let a = Array(first), b = Array(second)
        guard (4...64).contains(a.count), abs(a.count - b.count) <= 1 else { return false }
        if a.count == b.count {
            let differences = a.indices.filter { a[$0] != b[$0] }
            if differences.count == 1 { return true }
            guard differences.count == 2 else { return false }
            let i = differences[0], j = differences[1]
            return j == i + 1 && a[i] == b[j] && a[j] == b[i]
        }
        let shorter = a.count < b.count ? a : b
        let longer = a.count < b.count ? b : a
        let difference = shorter.indices.first { shorter[$0] != longer[$0] } ?? shorter.count
        return shorter[difference...].elementsEqual(longer[(difference + 1)...])
    }
}

struct SearchView: View {
    let player: PlayerStore
    @Bindable var navigation: AppNavigation
    @FocusState private var searchFocused: Bool

    private var input: SpotifySearchInput { SpotifySearchInput(navigation.searchText) }
    private var includesLocalMusic: Bool { navigation.searchKind == .searchAll || navigation.searchKind == .search }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 28) {
                HStack { Text("Find your next favorite.").font(.system(size: 36, weight: .medium, design: .serif)); Spacer() }
                HStack(spacing: 14) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("Songs, albums, podcasts or links…", text: $navigation.searchText)
                        .textFieldStyle(.plain).font(.title3)
                        .focused($searchFocused).accessibilityLabel("Search music")
                    if !navigation.searchText.isEmpty {
                        Button { navigation.searchText = ""; searchFocused = true } label: {
                            Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                        }.buttonStyle(.plain).accessibilityLabel("Clear search")
                    }
                }.padding(18).background(AuvraColors.cardEmphasis, in: RoundedRectangle(cornerRadius: 14))
                switch input {
                case .keywords(let query):
                    keywordResults(query)
                case .reference(let kind, let id):
                    Label("Shared link", systemImage: "link").font(.callout.weight(.medium)).foregroundStyle(.secondary)
                    SpotifyCatalogView(player: player, navigation: navigation, kind: kind, text: id)
                case .userProfile(let id):
                    SpotifyProfileLink(player: player, id: id).id(id)
                case .unsupportedLink:
                    ContentUnavailableView("Unsupported link", systemImage: "link",
                        description: Text("Paste an open.spotify.com content link or a Spotify URI."))
                }
            }.padding(38).frame(maxWidth: 1000, alignment: .leading).frame(maxWidth: .infinity)
        }
        .onAppear { searchFocused = true }
        .task(id: includesLocalMusic ? input : nil) {
            guard includesLocalMusic else { return }
            guard case .keywords(let query) = input else { return }
            do { try await Task.sleep(for: .milliseconds(200)) } catch { return }
            guard !Task.isCancelled else { return }
            player.browseLibrary(mode: .tracks, text: query, clearRoot: true)
        }
        .onChange(of: navigation.searchFocusRequest) { searchFocused = true }
    }

    @ViewBuilder private func keywordResults(_ query: String) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(RemoteCatalogKind.searchKinds) { kind in
                    Button { navigation.searchKind = kind } label: {
                        Text(kind.title).font(.callout.weight(.medium)).padding(.horizontal, 16).padding(.vertical, 10)
                            .background(navigation.searchKind == kind ? AuvraColors.accent.opacity(0.18) : AuvraColors.subtle, in: Capsule())
                            .foregroundStyle(navigation.searchKind == kind ? AuvraColors.accent : Color.primary)
                    }.buttonStyle(.plain).accessibilityAddTraits(navigation.searchKind == kind ? [.isSelected] : [])
                }
            }
        }
        SpotifyCatalogView(player: player, navigation: navigation, kind: navigation.searchKind, text: query)
        if includesLocalMusic, !query.isEmpty, player.libraryQuery.text == query,
           !player.isLoadingLibraryPage, (player.libraryPage?.total ?? 0) > 0 {
            Divider()
            Text("ALSO ON THIS MAC").font(.caption.weight(.semibold)).tracking(2).foregroundStyle(.secondary)
            LibraryResultsView(player: player)
        }
        if includesLocalMusic && !query.isEmpty {
            let results = player.localTracks.filter { item in
                ([item.title, item.artist, item.album ?? ""]).contains { $0.localizedStandardContains(query) }
            }
            if !results.isEmpty {
                Divider()
                Text("OPENED LOCAL FILES").font(.caption.weight(.semibold)).tracking(2).foregroundStyle(.secondary)
                ForEach(Array(results.enumerated()), id: \.element.id) { index, item in
                    TrackRow(item: item, index: index, active: player.current?.id == item.id) { player.play(item) }
                }
            }
        }
    }
}

private struct SpotifyProfileLink: View {
    let player: PlayerStore
    let id: String
    @Environment(\.openSettings) private var openSettings
    @AppStorage("AuvraSettingsTab") private var settingsTab = "general"
    @State private var requested = false

    private var uri: String { "spotify:user:\(id)" }
    private var connected: Bool { player.spotifyAccount.phase == .signedIn || player.spotifyState?.connection == .connected }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Spotify profile", systemImage: "person.crop.circle").font(.headline).accessibilityAddTraits(.isHeader)
            Text(id).foregroundStyle(.secondary).lineLimit(2).textSelection(.enabled)
            HStack {
                if connected {
                    Group {
                        if let follows = player.spotifySaved[uri] {
                            Button(follows ? "Unfollow" : "Follow") {
                                requested = true
                                player.saveSpotifyURI(uri, save: !follows)
                            }
                        } else {
                            Button("Check follow status") {
                                requested = true
                                player.checkSpotifyURI(uri)
                            }
                        }
                    }.disabled(player.spotifyEditPending)
                } else {
                    Button("Account Settings") { settingsTab = "account"; openSettings() }
                }
                if let url = URL(string: "https://open.spotify.com/user/\(id)") { Link("Open in Spotify", destination: url) }
            }
            if connected && requested { SpotifyEditFeedback(player: player) }
        }.padding(18).frame(maxWidth: .infinity, alignment: .leading)
            .background(AuvraColors.card, in: RoundedRectangle(cornerRadius: 16))
    }
}
