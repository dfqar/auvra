import AppKit
import SwiftUI
import Observation

private final class MiniPlayerPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}

@MainActor @Observable
final class MacWindowService: NSObject, WindowService, NSWindowDelegate {
    private(set) var miniVisible = false
    private(set) var mainFullScreen = false
    @ObservationIgnored private weak var player: PlayerStore?
    @ObservationIgnored private weak var navigation: AppNavigation?
    @ObservationIgnored private weak var mainWindow: NSWindow?
    @ObservationIgnored private var miniPanel: NSPanel?
    @ObservationIgnored private var openMain: (() -> Void)?
    @ObservationIgnored private var keyboard: MacKeyboardShortcuts?
    @ObservationIgnored private var preferencesObserver: NSObjectProtocol?
    @ObservationIgnored private var observers: [NSObjectProtocol] = []
    func connect(player: PlayerStore, navigation: AppNavigation, openMain: @escaping () -> Void) {
        self.player = player; self.navigation = navigation; self.openMain = openMain
        if preferencesObserver == nil {
            preferencesObserver = NotificationCenter.default.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.updateMiniLevel() }
            }
        }
        if keyboard == nil { keyboard = MacKeyboardShortcuts(player: player, exitListening: { [weak self] window in
            guard let self, window === self.mainWindow, self.navigation?.page == .nowPlaying else { return false }
            self.navigation?.leaveNowPlaying(); return true
        }) { [weak self] in self?.owns($0) == true } }
    }
    func registerMain(_ window: NSWindow) {
        guard mainWindow !== window else { return }
        for observer in observers { NotificationCenter.default.removeObserver(observer) }; observers.removeAll()
        window.setFrameAutosaveName("AuvraMainPlayer")
        mainWindow = window; mainFullScreen = window.styleMask.contains(.fullScreen)
        for name in [NSWindow.didEnterFullScreenNotification, NSWindow.didExitFullScreenNotification] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self, weak window] _ in
                MainActor.assumeIsolated { self?.mainFullScreen = window?.styleMask.contains(.fullScreen) == true }
            })
        }
    }
    private func owns(_ window: NSWindow) -> Bool { window === mainWindow || window === miniPanel }
    private func updateMiniLevel() {
        let onTop = UserDefaults.standard.object(forKey: "AuvraMiniAlwaysOnTop") as? Bool ?? true
        miniPanel?.level = onTop ? .floating : .normal
        miniPanel?.appearance = AppAppearance.current.nativeAppearance
    }
    func showMiniPlayer() {
        guard let player else { return }
        if miniPanel == nil {
            let panel = MiniPlayerPanel(contentRect: NSRect(x: 0, y: 0, width: 360, height: 264),
                                        styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
            panel.title = "Auvra Mini Player"; panel.titleVisibility = .hidden
            panel.titlebarAppearsTransparent = true; panel.isReleasedWhenClosed = false
            panel.animationBehavior = .utilityWindow
            panel.isFloatingPanel = true; panel.level = .floating; panel.hidesOnDeactivate = false
            panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            panel.appearance = AppAppearance.current.nativeAppearance
            panel.contentView = NSHostingView(rootView: MiniPlayerView(player: player, showMain: { [weak self] in self?.showMainPlayer() }))
            panel.setContentSize(NSSize(width: 360, height: 264)); panel.center()
            panel.setFrameAutosaveName("AuvraMiniPlayer")
            panel.delegate = self; miniPanel = panel
        }
        updateMiniLevel()
        miniVisible = true; miniPanel?.makeKeyAndOrderFront(nil); NSApp.activate()
    }
    func showMainPlayer() {
        miniPanel?.orderOut(nil); miniVisible = false
        openMain?()
        if let mainWindow {
            if mainWindow.isMiniaturized { mainWindow.deminiaturize(nil) }
            mainWindow.makeKeyAndOrderFront(nil)
        }
        NSApp.activate()
    }
    func toggleMiniPlayer() {
        if miniVisible { miniPanel?.orderOut(nil); miniVisible = false }
        else { showMiniPlayer() }
    }
    func toggleFullScreenNowPlaying() {
        let wasNowPlaying = navigation?.page == .nowPlaying
        navigation?.show(.nowPlaying); showMainPlayer()
        if let mainWindow, !mainWindow.styleMask.contains(.fullScreen) || wasNowPlaying { mainWindow.toggleFullScreen(nil) }
    }
    func windowWillClose(_ notification: Notification) {
        if let window = notification.object as? NSWindow, window === miniPanel { miniVisible = false }
    }
    func showKeyboardShortcuts() {
        let alert = NSAlert(); alert.messageText = "Auvra keyboard shortcuts"
        alert.informativeText = "Space · Play / Pause\n⌘→ / ⌘← · Next / Previous\n⌥⌘→ / ⌥⌘← · Seek by the current skip interval (10 seconds by default)\n⌘↑ / ⌘↓ · Volume\n⇧⌘S · Shuffle\n⌘R · Cycle repeat\n⌘O · Open audio file\n⇧⌘O · Add music folder\n⌘L · Search\n⌘1 / ⌘2 / ⌘3 · Library / Queue / Now Playing\n⇧⌘M · Mini Player\n⌘0 · Main Player\n⌘F · Full Screen Now Playing\n⌘. · Stop\n\nSpace remains normal text input while you’re typing."
        alert.addButton(withTitle: "Done")
        if let window = NSApp.keyWindow ?? mainWindow { alert.beginSheetModal(for: window) }
    }
    func shutdown() {
        keyboard?.uninstall(); keyboard = nil
        if let preferencesObserver { NotificationCenter.default.removeObserver(preferencesObserver) }; preferencesObserver = nil
        for observer in observers { NotificationCenter.default.removeObserver(observer) }; observers.removeAll()
        miniPanel?.orderOut(nil); miniPanel = nil; miniVisible = false; openMain = nil
    }
}

struct MainWindowReader: NSViewRepresentable {
    let found: (NSWindow) -> Void
    func makeNSView(context: Context) -> NSView { let view = WindowProbe(); view.found = found; return view }
    func updateNSView(_ view: NSView, context: Context) { (view as? WindowProbe)?.found = found }
    private final class WindowProbe: NSView {
        var found: ((NSWindow) -> Void)?
        override func viewDidMoveToWindow() { super.viewDidMoveToWindow(); if let window { found?(window) } }
    }
}
