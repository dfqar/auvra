import Observation
import SwiftUI

/// Project only the labels and affordances the menu bar uses. Playback snapshots
/// carry clock and queue data that can change without changing any menu item.
@MainActor @Observable
final class AuvraMenuPresentation {
    struct Source: Equatable {
        let name: String
        let url: URL?
        let precision: String
    }
    struct State: Equatable {
        var choosingFolder = false
        var choosingFile = false
        var playbackToggleShowsPause = false
        var canTogglePlayback = false
        var canRetry = false
        var canStop = false
        var canGoNext = false
        var canGoPrevious = false
        var canSeek = false
        var forwardSeconds: Double = 10
        var backwardSeconds: Double = 10
        var canShuffle = false
        var shuffle = false
        var canRepeat = false
        var repeatMode: RepeatMode?
        var canIncreaseVolume = false
        var canDecreaseVolume = false
        var hasTrack = false
        var lyricsVisible = false
        var lyricsSource: Source?
        var lyricsImporting = false
        var lyricsLoading = false
        var lyricsTimed = false
        var lyricsFollowing = true
        var lyricsOffset: Double = 0
        var miniVisible = false
        var fullScreenNowPlaying = false
    }

    private(set) var state = State()
    @ObservationIgnored private weak var player: PlayerStore?
    @ObservationIgnored private weak var navigation: AppNavigation?
    @ObservationIgnored private weak var windows: MacWindowService?
    @ObservationIgnored private var observation = 0
    @ObservationIgnored private var previousThresholdTask: Task<Void, Never>?

    func connect(player: PlayerStore, navigation: AppNavigation, windows: MacWindowService) {
        guard self.player !== player || self.navigation !== navigation || self.windows !== windows else { return }
        self.player = player
        self.navigation = navigation
        self.windows = windows
        refresh()
    }

    private func refresh() {
        previousThresholdTask?.cancel()
        previousThresholdTask = nil
        guard let player, let navigation, let windows else { return }
        observation &+= 1
        let generation = observation
        let projection = withObservationTracking {
            // A tracked menu must keep the same item graph until it closes.
            // While open, observe only the tracking boundary, not the playback clock.
            guard !NativeMenuTracking.shared.active else { return nil as (State, Duration?)? }
            let delay = Self.previousThresholdDelay(player)
            return (Self.project(player: player, navigation: navigation, windows: windows), delay)
        } onChange: { [weak self] in
            Task { @MainActor [weak self] in
                guard let self, self.observation == generation else { return }
                self.refresh()
            }
        }
        guard let (next, delay) = projection else { return }
        if state != next { state = next }
        if let delay {
            previousThresholdTask = Task { @MainActor [weak self] in
                do { try await Task.sleep(for: delay) } catch { return }
                self?.refresh()
            }
        }
    }

    private static func project(player: PlayerStore, navigation: AppNavigation, windows: MacWindowService) -> State {
        let lyrics = player.lyrics
        let canChangeVolume = player.snapshot?.capabilities.volume == true
        let value = player.volume
        var result = State(
            choosingFolder: player.isChoosingFolder, choosingFile: player.isChoosingFile,
            playbackToggleShowsPause: player.playbackToggleShowsPause, canTogglePlayback: player.canTogglePlayback,
            canRetry: player.canRetryPlayback, canStop: player.canStop,
            canGoNext: player.canGoNext, canGoPrevious: player.canGoPrevious, canSeek: player.canSeek,
            forwardSeconds: player.skipForwardSeconds, backwardSeconds: player.skipBackwardSeconds,
            canShuffle: player.canShuffle, shuffle: player.shuffleEnabled,
            canRepeat: player.canRepeat, repeatMode: player.repeatMode,
            canIncreaseVolume: canChangeVolume && value < 1, canDecreaseVolume: canChangeVolume && value > 0,
            hasTrack: player.current != nil, lyricsVisible: lyrics.isVisible,
            miniVisible: windows.miniVisible, fullScreenNowPlaying: windows.mainFullScreen && navigation.page == .nowPlaying
        )
        if result.lyricsVisible {
            result.lyricsSource = lyrics.result.map { Source(name: $0.source, url: $0.sourceURL, precision: $0.document.precision) }
            result.lyricsImporting = lyrics.isImporting
            result.lyricsLoading = lyrics.isLoading
            result.lyricsTimed = lyrics.result?.document.hasTiming == true
            result.lyricsFollowing = lyrics.followsPlayback
            result.lyricsOffset = lyrics.offsetSeconds
        }
        return result
    }

    private static func previousThresholdDelay(_ player: PlayerStore) -> Duration? {
        let threshold = PlayerStore.previousRestartMilliseconds
        guard player.isPlaying, let current = player.current, current.durationMs >= threshold,
              let snapshot = player.snapshot, snapshot.providerConnection == .connected || snapshot.playbackSessionActive else {
            return nil
        }
        let canSkipBackward = player.canNavigateQueue && snapshot.transport.previous == true
        guard canSkipBackward != player.canSeek else { return nil }
        let remaining = Double(threshold) - player.positionMilliseconds()
        let rate = snapshot.playbackRate.flatMap { $0.isFinite && (0...4).contains($0) ? $0 : nil } ?? 1
        guard remaining > 0, rate > 0 else { return nil }
        // Waking up just after the boundary also handles extrapolated clocks
        // when the provider has not emitted a new snapshot.
        let seconds = remaining / rate / 1000
        return .seconds(min(seconds + 0.001, 86_400))
    }
}

struct AuvraCommands: Commands {
    let player: PlayerStore
    let navigation: AppNavigation
    let windows: MacWindowService
    let presentation: AuvraMenuPresentation
    var body: some Commands {
        let state = presentation.state
        CommandGroup(replacing: .newItem) {
            Button("Add Music Folder…") { Task { await player.chooseLibraryFolder() } }
                .keyboardShortcut("o", modifiers: [.command, .shift]).disabled(state.choosingFolder)
            Button("Open Audio File…") { Task { await player.chooseFile() } }
                .keyboardShortcut("o", modifiers: .command).disabled(state.choosingFile)
        }
        CommandMenu("Playback") {
            Button(state.playbackToggleShowsPause ? "Pause" : "Play", action: player.togglePlayback)
                .disabled(!state.canTogglePlayback).help("Play or pause · Space")
            Button("Retry Playback", action: player.retryPlayback).disabled(!state.canRetry)
            Button("Stop", action: player.stop).keyboardShortcut(".", modifiers: .command).disabled(!state.canStop)
            Button("Next", action: player.next).keyboardShortcut(.rightArrow, modifiers: .command).disabled(!state.canGoNext)
            Button("Previous", action: player.previous).keyboardShortcut(.leftArrow, modifiers: .command).disabled(!state.canGoPrevious)
            Divider()
            Button("Seek Forward \(state.forwardSeconds.formatted()) Seconds") { player.seekRelative(seconds: player.skipForwardSeconds) }
                .keyboardShortcut(.rightArrow, modifiers: [.command, .option]).disabled(!state.canSeek)
            Button("Seek Backward \(state.backwardSeconds.formatted()) Seconds") { player.seekRelative(seconds: -player.skipBackwardSeconds) }
                .keyboardShortcut(.leftArrow, modifiers: [.command, .option]).disabled(!state.canSeek)
            Divider()
            Toggle("Shuffle", isOn: Binding(get: { state.shuffle }, set: { player.send(.setShuffle($0)) }))
                .keyboardShortcut("s", modifiers: [.command, .shift]).disabled(!state.canShuffle)
            Menu("Repeat") {
                repeatChoice("Off", .off, selected: state.repeatMode)
                repeatChoice("All", .all, selected: state.repeatMode)
                repeatChoice("One", .one, selected: state.repeatMode)
                Divider()
                Button("Cycle Repeat", action: player.cycleRepeat).keyboardShortcut("r", modifiers: .command)
            }.disabled(!state.canRepeat)
            Divider()
            Button("Volume Up") { player.adjustVolume(0.05) }.keyboardShortcut(.upArrow, modifiers: .command)
                .disabled(!state.canIncreaseVolume)
            Button("Volume Down") { player.adjustVolume(-0.05) }.keyboardShortcut(.downArrow, modifiers: .command)
                .disabled(!state.canDecreaseVolume)
        }
        CommandGroup(after: .sidebar) {
            Button(state.lyricsVisible ? "Hide Lyrics" : "Show Lyrics") { windows.showMainPlayer(); navigation.showLyrics(player: player) }
                .keyboardShortcut("l", modifiers: [.command, .shift]).disabled(!state.hasTrack)
            lyricsOptions(state)
            Button("Home") { windows.showMainPlayer(); navigation.show(.home) }
            Button("Your Library") { windows.showMainPlayer(); navigation.show(.library) }.keyboardShortcut("1", modifiers: .command)
            Button("Liked Songs") { windows.showMainPlayer(); navigation.show(.likedSongs) }
            Button("Local Files") { windows.showMainPlayer(); navigation.show(.localLibrary) }
            Button("Queue") { windows.showMainPlayer(); navigation.show(.queue) }.keyboardShortcut("2", modifiers: .command)
            Button("Now Playing") { windows.showMainPlayer(); navigation.show(.nowPlaying) }.keyboardShortcut("3", modifiers: .command)
            Button("Search Music") { windows.showMainPlayer(); navigation.focusSearch() }.keyboardShortcut("l", modifiers: .command)
            Divider()
            Button("Show Main Player", action: windows.showMainPlayer).keyboardShortcut("0", modifiers: .command)
            Button(state.miniVisible ? "Hide Mini Player" : "Show Mini Player", action: windows.toggleMiniPlayer)
                .keyboardShortcut("m", modifiers: [.command, .shift])
            Button(state.fullScreenNowPlaying ? "Exit Full Screen Now Playing" : "Full Screen Now Playing", action: windows.toggleFullScreenNowPlaying)
                .keyboardShortcut("f", modifiers: .command)
        }
        CommandGroup(replacing: .help) {
            Button("Auvra Keyboard Shortcuts", action: windows.showKeyboardShortcuts)
        }
    }
    private func lyricsOptions(_ state: AuvraMenuPresentation.State) -> some View {
        let lyrics = player.lyrics
        return Menu("Lyrics Options") {
            if let source = state.lyricsSource {
                if let url = source.url { Link(source.name, destination: url) }
                else { Text(source.name) }
                Text(source.precision == "word" ? "Word sync" : source.precision == "line" ? "Line sync" : "Plain text")
                Divider()
            }
            Button("Import Lyrics…") { Task { await lyrics.importLyrics() } }.disabled(state.lyricsImporting)
            Button("Try Online Lyrics Again", action: lyrics.load).disabled(state.lyricsLoading)
            if state.lyricsTimed {
                Divider()
                Toggle("Follow Playback", isOn: Binding(get: { state.lyricsFollowing }, set: { lyrics.followsPlayback = $0 }))
                Button("Lyrics Earlier · 0.25 s") { lyrics.adjustOffset(0.25) }
                Button("Lyrics Later · 0.25 s") { lyrics.adjustOffset(-0.25) }
                Button("Reset Timing") { lyrics.offsetSeconds = 0 }.disabled(state.lyricsOffset == 0)
                if state.lyricsOffset != 0 { Text(String(format: "Timing %+.2f s", state.lyricsOffset)) }
            }
        }.disabled(!state.lyricsVisible)
    }
    private func repeatChoice(_ title: String, _ mode: RepeatMode, selected: RepeatMode?) -> some View {
        Toggle(title, isOn: Binding(get: { selected == mode }, set: { if $0 { player.send(.setRepeat(mode)) } }))
    }
}
