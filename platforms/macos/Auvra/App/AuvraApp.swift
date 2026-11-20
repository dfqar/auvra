import SwiftUI

@main
enum AuvraEntry {
    @MainActor
    static func main() {
        do {
            switch try ApplicationSessionLock.acquire(in: SoloistRuntimeInstaller.supportDirectory) {
            case .primary(let lease):
                // A running copy from before the lock was introduced still owns
                // its open library. Let it finish before starting this version.
                if let running = otherInstances().first(where: \.isFinishedLaunching) {
                    reopen(running)
                } else {
                    withExtendedLifetime(lease) { AuvraApp.main() }
                }
            case .occupied(let pid):
                let running = otherInstances()
                if let owner = running.first(where: { $0.processIdentifier == pid }) ?? running.first {
                    reopen(owner)
                }
            }
        } catch {
            let app = NSApplication.shared
            app.setActivationPolicy(.regular); app.activate()
            let alert = NSAlert()
            alert.messageText = "Auvra couldn’t open its session"
            alert.informativeText = "Check that your Auvra Application Support folder is accessible, then open Auvra again."
            alert.addButton(withTitle: "Close")
            alert.runModal()
        }
    }

    @MainActor
    private static func otherInstances() -> [NSRunningApplication] {
        guard let identifier = Bundle.main.bundleIdentifier else { return [] }
        return NSRunningApplication.runningApplications(withBundleIdentifier: identifier)
            .filter { $0.processIdentifier != ProcessInfo.processInfo.processIdentifier && !$0.isTerminated }
    }

    @MainActor
    private static func reopen(_ owner: NSRunningApplication) {
        owner.activate()
        guard let url = owner.bundleURL else { return }
        let configuration = NSWorkspace.OpenConfiguration()
        configuration.activates = true
        configuration.createsNewApplicationInstance = false
        configuration.promptsUserIfNeeded = false
        let completion = DispatchGroup()
        completion.enter()
        NSWorkspace.shared.openApplication(at: url, configuration: configuration) { _, _ in completion.leave() }
        let deadline = Date().addingTimeInterval(0.75)
        while completion.wait(timeout: .now()) == .timedOut && Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
    }
}

struct AuvraApp: App {
    @NSApplicationDelegateAdaptor(AuvraApplicationDelegate.self) private var appDelegate
    @State private var player = PlayerStore(loadSpotify: true)
    @State private var navigation = AppNavigation()
    @State private var still = StillStore()
    @State private var barLighting = StillStore(compact: true)
    @State private var windows = MacWindowService()
    @State private var menuPresentation = AuvraMenuPresentation()
    @State private var systemMedia: MediaPlayerSystemSession?

    var body: some Scene {
        Window("Auvra", id: "main") {
            RootView(player: player, navigation: navigation, still: still, windows: windows, barLighting: barLighting)
                .onAppear {
                    appDelegate.player = player; appDelegate.windows = windows; appDelegate.still = still
                    appDelegate.barLighting = barLighting
                    menuPresentation.connect(player: player, navigation: navigation, windows: windows)
                    player.attachArtworkPalette { [weak still, weak barLighting] value in
                        still?.setPalette(value?.colors)
                        barLighting?.setPalette(value?.colors)
                    }
                    player.attachVisualModulation { [weak still] value in
                        still?.setModulation(value.map { StillModulationInput(timeSeconds: $0.controls.timeSeconds,
                            motion: $0.controls.motion, turbulence: $0.controls.turbulence, light: $0.controls.light) })
                    }
                    player.spotifyVisibility(NSApp.isActive)
                    if systemMedia == nil {
                        let session = MediaPlayerSystemSession(player: player)
                        systemMedia = session; player.attachSystemMedia(session)
                    }
                }
                .onOpenURL { player.openFile($0); navigation.show(.localLibrary) }
        }
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 1180, height: 780)
        .windowResizability(.contentMinSize)
        .commands { AuvraCommands(player: player, navigation: navigation, windows: windows, presentation: menuPresentation) }
        Settings { SettingsView(player: player, still: still) }
    }
}

@MainActor
final class AuvraApplicationDelegate: NSObject, NSApplicationDelegate {
    weak var player: PlayerStore?
    weak var windows: MacWindowService?
    weak var still: StillStore?
    weak var barLighting: StillStore?
    private var closing = false
    func applicationDidBecomeActive(_ notification: Notification) { player?.spotifyVisibility(true) }
    func applicationDidResignActive(_ notification: Notification) { player?.spotifyVisibility(false) }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        guard let windows else { return true }
        windows.showMainPlayer()
        return false
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let player else { return .terminateNow }
        if !closing {
            closing = true; windows?.shutdown()
            Task {
                await player.prepareToQuit()
                await barLighting?.shutdown()
                await still?.shutdown()
                sender.reply(toApplicationShouldTerminate: true)
            }
        }
        return .terminateLater
    }
}
