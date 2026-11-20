import Observation

enum AppPage: String, CaseIterable, Identifiable {
    case home, library, likedSongs, search, queue, nowPlaying, localLibrary, collection
    var id: Self { self }
    var title: String {
        switch self {
        case .home: "Home"
        case .library: "Your Library"
        case .likedSongs: "Liked Songs"
        case .search: "Search"
        case .queue: "Queue"
        case .nowPlaying: "Now Playing"
        case .localLibrary: "Local Files"
        case .collection: "Collection"
        }
    }
    var symbol: String {
        switch self {
        case .home: "house"
        case .library: "square.stack"
        case .likedSongs: "heart.fill"
        case .search: "magnifyingglass"
        case .queue: "list.bullet"
        case .nowPlaying: "sparkles"
        case .localLibrary: "folder"
        case .collection: "square.stack"
        }
    }
}

/// Window navigation is UI state, independent of the shared playback snapshot.
@MainActor @Observable
final class AppNavigation {
    var selection: AppPage? = .home
    var searchText = ""
    var searchKind: RemoteCatalogKind = .searchAll
    var libraryKind: RemoteCatalogKind = .playlists
    var listeningPeriod = "medium_term"
    private var collectionReturnPage: AppPage = .home
    private var collectionHistory: [RemoteCollection] = []
    var selectedCollection: RemoteCollection?
    private var listeningReturnPage: AppPage = .home
    private(set) var searchFocusRequest = 0
    var page: AppPage { selection ?? .home }

    func show(_ page: AppPage) {
        if page == .nowPlaying && self.page != .nowPlaying { listeningReturnPage = self.page }
        selection = page
    }
    func showLyrics(player: PlayerStore) {
        player.lyrics.setVisible(!player.lyrics.isVisible)
        if player.lyrics.isVisible { show(.nowPlaying) }
    }
    func leaveNowPlaying() { selection = listeningReturnPage }
    func openCollection(_ collection: RemoteCollection) {
        if page == .collection, let selectedCollection { collectionHistory.append(selectedCollection); if collectionHistory.count > 32 { collectionHistory.removeFirst() } }
        else { collectionReturnPage = page; collectionHistory.removeAll() }
        selectedCollection = collection; selection = .collection
    }
    func refreshCollection(_ update: RemoteCollection) {
        func merged(_ old: RemoteCollection) -> RemoteCollection {
            RemoteCollection(id: update.id, title: update.title, url: update.url, artwork: update.artwork ?? old.artwork, isPublic: update.isPublic ?? old.isPublic, kind: update.kind ?? old.kind, subtitle: update.subtitle ?? old.subtitle, description: update.description ?? old.description, ownerId: update.ownerId ?? old.ownerId, collaborative: update.collaborative ?? old.collaborative)
        }
        if let old = selectedCollection, old.id == update.id { selectedCollection = merged(old) }
    }
    func leaveCollection() {
        if let previous = collectionHistory.popLast() { selectedCollection = previous }
        else { selectedCollection = nil; selection = collectionReturnPage }
    }
    func focusSearch() { selection = .search; searchFocusRequest += 1 }
}
