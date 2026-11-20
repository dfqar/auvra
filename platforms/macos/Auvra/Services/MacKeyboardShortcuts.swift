import AppKit
import Observation
import OSLog

@MainActor @Observable
final class NativeMenuTracking {
    static let shared = NativeMenuTracking()
    private(set) var active = false
    @ObservationIgnored private var menus: Set<ObjectIdentifier> = []
    @ObservationIgnored private var observers: [NSObjectProtocol] = []

    private init() {
        let notifications = NotificationCenter.default
        for name in [NSMenu.didBeginTrackingNotification, NSMenu.didEndTrackingNotification] {
            observers.append(notifications.addObserver(forName: name, object: nil, queue: .main) { [weak self] notification in
                guard let menu = notification.object as? NSMenu else { return }
                let identifier = ObjectIdentifier(menu)
                MainActor.assumeIsolated {
                    guard let self else { return }
                    if name == NSMenu.didBeginTrackingNotification { self.menus.insert(identifier) }
                    else { self.menus.remove(identifier) }
                    let active = !self.menus.isEmpty
                    if self.active != active { self.active = active }
                }
            })
        }
        observers.append(notifications.addObserver(forName: NSApplication.didResignActiveNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.active else { return }
                self.menus.removeAll()
                self.active = false
            }
        })
    }
}

enum PlaybackNavigationKey {
    case next, previous
}

/// Explicit app presentations get Escape before the underlying listening page.
@MainActor
final class NativeTransientPresentations {
    static let shared = NativeTransientPresentations()
    private struct Entry {
        weak var window: NSWindow?
        let dismiss: () -> Void
    }
    private var entries: [ObjectIdentifier: Entry] = [:]

    func set(owner: AnyObject, window: NSWindow?, dismiss: (() -> Void)?) {
        let id = ObjectIdentifier(owner)
        if let window, let dismiss {
            entries[id] = Entry(window: window, dismiss: dismiss)
        } else {
            entries.removeValue(forKey: id)
        }
    }

    func dismiss(in window: NSWindow) -> Bool {
        entries = entries.filter { $0.value.window != nil }
        guard let (id, entry) = entries.first(where: { $0.value.window === window }) else { return false }
        entries.removeValue(forKey: id)
        entry.dismiss()
        return true
    }
}

/// App-local transport and listening shortcuts. Editing and native dialogs retain their own key handling.
@MainActor
final class MacKeyboardShortcuts {
    private static let tracePlayback = ProcessInfo.processInfo.environment["AUVRA_AUDIO_TRACE"] == "1"
    private static let playbackTiming = Logger(subsystem: "app.auvra.player", category: "PlaybackTiming")
    private var monitor: Any?
    private weak var player: PlayerStore?
    private let exitListening: (NSWindow) -> Bool
    private let ownsWindow: (NSWindow) -> Bool
    init(player: PlayerStore, exitListening: @escaping (NSWindow) -> Bool = { _ in false }, ownsWindow: @escaping (NSWindow) -> Bool) {
        self.player = player; self.ownsWindow = ownsWindow; self.exitListening = exitListening
        let menuTracking = NativeMenuTracking.shared
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            let handled = MainActor.assumeIsolated {
                if Self.tracePlayback, event.keyCode == 123 || event.keyCode == 124 {
                    let direction = event.keyCode == 124 ? "next" : "previous"
                    let command = event.modifierFlags.contains(.command)
                    let owned = NSApp.keyWindow.map { self?.ownsWindow($0) == true } == true
                    let age = (ProcessInfo.processInfo.systemUptime - event.timestamp) * 1000
                    Self.playbackTiming.info("navigation_key direction=\(direction, privacy: .public) command=\(command, privacy: .public) window_owned=\(owned, privacy: .public) menu_tracking=\(menuTracking.active, privacy: .public) event_age_ms=\(age, privacy: .public)")
                }
                guard let self, !menuTracking.active, RunLoop.current.currentMode != .eventTracking,
                      let window = NSApp.keyWindow, self.ownsWindow(window), let player = self.player else { return false }
                let editing = (window.firstResponder as? NSTextView)?.isEditable == true || (window.firstResponder as? NSTextField)?.isEditable == true
                let modal = NSApp.modalWindow != nil || window.sheetParent != nil || window.attachedSheet != nil
                if Self.handleEscape(keyCode: event.keyCode, flags: event.modifierFlags, editing: editing, modal: modal,
                                     dismissPresentation: { NativeTransientPresentations.shared.dismiss(in: window) },
                                     exitListening: { self.exitListening(window) }) { return true }
                if event.keyCode == 123 || event.keyCode == 124,
                   self.handleNavigation(event, in: window, player: player, editing: editing, modal: modal) { return true }
                guard Self.routesSpace(keyCode: event.keyCode, flags: event.modifierFlags,
                                       editing: editing, modal: modal),
                      player.canTogglePlayback else { return false }
                if !event.isARepeat { player.togglePlayback() }
                return true
            }
            return handled ? nil : event
        }
    }
    private func handleNavigation(_ event: NSEvent, in window: NSWindow, player: PlayerStore, editing: Bool, modal: Bool) -> Bool {
        guard NSApp.isActive, event.window == nil || event.window === window else { return false }
        let editor = (window.firstResponder as? NSTextView)
            ?? (window.firstResponder as? NSTextField)?.currentEditor() as? NSTextView
        if let editor, editor.isEditable || editor.isSelectable,
           Self.navigation(keyCode: event.keyCode, flags: event.modifierFlags, editing: false, modal: modal) != nil {
            // Key equivalents run before the field editor's ordinary key handling.
            editor.keyDown(with: event)
            return true
        }
        // AppKit retains custom and mirrored menu equivalents and key repeat.
        guard !event.isARepeat, NSApp.userInterfaceLayoutDirection == .leftToRight,
              UserDefaults.standard.dictionary(forKey: "NSUserKeyEquivalents")?.isEmpty != false,
              let key = Self.navigation(keyCode: event.keyCode, flags: event.modifierFlags, editing: editing, modal: modal) else { return false }
        switch key {
        case .next where player.canGoNext: player.next()
        case .previous where player.canGoPrevious: player.previous()
        default: return false
        }
        return true
    }
    nonisolated static func navigation(keyCode: UInt16, flags: NSEvent.ModifierFlags, editing: Bool, modal: Bool) -> PlaybackNavigationKey? {
        guard !editing, !modal, flags.intersection([.command, .control, .option, .shift]) == .command else { return nil }
        switch keyCode {
        case 124: return .next
        case 123: return .previous
        default: return nil
        }
    }
    nonisolated static func routesSpace(keyCode: UInt16, flags: NSEvent.ModifierFlags, editing: Bool, modal: Bool) -> Bool {
        keyCode == 49 && flags.intersection([.command, .control, .option, .shift]).isEmpty && !editing && !modal
    }
    nonisolated static func routesEscape(keyCode: UInt16, flags: NSEvent.ModifierFlags, editing: Bool, modal: Bool) -> Bool {
        keyCode == 53 && flags.intersection([.command, .control, .option, .shift]).isEmpty && !editing && !modal
    }
    static func handleEscape(keyCode: UInt16, flags: NSEvent.ModifierFlags, editing: Bool, modal: Bool,
                             dismissPresentation: () -> Bool, exitListening: () -> Bool) -> Bool {
        guard routesEscape(keyCode: keyCode, flags: flags, editing: editing, modal: modal) else { return false }
        return dismissPresentation() || exitListening()
    }
    func uninstall() {
        if let monitor { NSEvent.removeMonitor(monitor); self.monitor = nil }
    }
}
