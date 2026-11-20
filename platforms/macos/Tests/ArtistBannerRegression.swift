import Foundation

/// Palette work never prepares a GPU device. Rendering is covered by the artist
/// page's normal native UI pass; these checks exercise async ownership and culling.
enum ArtistBannerRegression {
    @MainActor
    private final class PaletteRequests {
        var waiting: [String: CheckedContinuation<ArtworkPalette?, Never>] = [:]
        var entered: Set<String> = []
        func load(_ reference: String) async -> ArtworkPalette? {
            await withCheckedContinuation { waiting[reference] = $0 }
        }
        func complete(_ reference: String, _ palette: ArtworkPalette?) {
            waiting.removeValue(forKey: reference)?.resume(returning: palette)
        }
        func waitFor(_ reference: String) async -> Bool {
            for _ in 0..<100 {
                if waiting[reference] != nil { return true }
                await Task.yield()
            }
            return false
        }
        func waitForEntry(_ reference: String) async -> Bool {
            for _ in 0..<100 {
                if entered.contains(reference) { return true }
                await Task.yield()
            }
            return false
        }
    }

    @MainActor
    static func run(check: (Bool, String) throws -> Void) async throws {
        let red = ArtworkPalette(colors: [[0.84, 0.13, 0.08], [0.55, 0.19, 0.10], [0.24, 0.09, 0.05]])
        let blue = ArtworkPalette(colors: [[0.10, 0.37, 0.81], [0.12, 0.65, 0.73], [0.07, 0.16, 0.34]])
        let requests = PaletteRequests()
        let model = ArtistBannerLighting(loader: { await requests.load($0) })
        try check(model.still.engine == nil && !model.still.isPreparing,
                  "Artist banner starts without preparing a graphics device")
        try check(!model.still.hasPalette && model.still.displayPalette.allSatisfy { $0[0] == $0[1] && $0[1] == $0[2] },
                  "An artist with no decoded avatar starts with a neutral palette")

        let first = Task { @MainActor in await model.load("avatar-first") }
        try check(await requests.waitFor("avatar-first"), "The first avatar palette request is admitted")
        let second = Task { @MainActor in await model.load("avatar-second") }
        try check(await requests.waitFor("avatar-second"), "A newer artist supersedes a pending palette")
        requests.complete("avatar-second", blue)
        await second.value
        try check(model.still.displayPalette == blue.colors, "The banner uses the current artist's decoded avatar colors")
        requests.complete("avatar-first", red)
        await first.value
        try check(model.still.displayPalette == blue.colors, "A late previous artist cannot recolor the current banner")

        let hidden = Task { @MainActor in await model.load("avatar-hidden") }
        try check(await requests.waitFor("avatar-hidden"), "A pending avatar request can be cancelled on page disappearance")
        model.cancel()
        requests.complete("avatar-hidden", red)
        await hidden.value
        try check(model.still.displayPalette == blue.colors, "A disappeared page rejects a late palette result")

        let cancelled = Task { @MainActor in await model.load("avatar-cancelled") }
        try check(await requests.waitFor("avatar-cancelled"), "The view task remains independently cancellable")
        cancelled.cancel()
        requests.complete("avatar-cancelled", red)
        await cancelled.value
        try check(model.still.displayPalette == blue.colors, "A cancelled view task cannot publish avatar colors")

        let fresh = Task { @MainActor in await model.load("avatar-current") }
        try check(await requests.waitFor("avatar-current"), "A current avatar can wait while an older view task is already cancelled")
        let beforeNil = Task { @MainActor in await model.load(nil) }
        beforeNil.cancel()
        await beforeNil.value
        try check(model.still.displayPalette == blue.colors, "A task cancelled before entry cannot clear the current palette")
        requests.complete("avatar-current", red)
        await fresh.value
        try check(model.still.displayPalette == red.colors, "A cancelled nil request cannot invalidate a newer pending avatar")

        let latest = Task { @MainActor in await model.load("avatar-latest") }
        try check(await requests.waitFor("avatar-latest"), "The latest avatar owns its async generation")
        let beforeReference = Task { @MainActor in
            requests.entered.insert("cancelled-before-entry")
            await model.load("cancelled-before-entry")
        }
        beforeReference.cancel()
        try check(await requests.waitForEntry("cancelled-before-entry"), "The pre-cancelled reference task reached the palette boundary")
        let wronglyAdmitted = requests.waiting["cancelled-before-entry"] != nil
        if wronglyAdmitted { requests.complete("cancelled-before-entry", nil) }
        await beforeReference.value
        try check(!wronglyAdmitted, "A task cancelled before entry never asks the artwork service to load")
        requests.complete("avatar-latest", blue)
        await latest.value
        try check(model.still.displayPalette == blue.colors, "A pre-cancelled old reference cannot supersede the current generation")

        await model.load(nil)
        try check(!model.still.hasPalette, "A missing avatar returns to neutral rather than a builtin scene preset")
        try check(model.still.engine == nil && !model.still.isPreparing,
                  "Avatar palette lookup alone never allocates a Metal engine")

        let size = CGSize(width: 760, height: 268)
        let visible: [(CGRect?, Bool, String)] = [
            (CGRect(x: 0, y: 0, width: 760, height: 600), true, "fully visible"),
            (CGRect(x: 0, y: 200, width: 760, height: 600), true, "partly visible"),
            (CGRect(x: 0, y: -590, width: 760, height: 600), true, "ten points visible"),
            (CGRect(x: 0, y: 268, width: 760, height: 600), false, "just below the viewport"),
            (CGRect(x: 0, y: -600, width: 760, height: 600), false, "just above the viewport"),
            (CGRect(x: 0, y: 400, width: 760, height: 600), false, "scrolled completely away"),
            (CGRect(x: 760, y: 0, width: 760, height: 600), false, "outside horizontally"),
            (.zero, false, "empty viewport"),
            (.null, false, "unresolved viewport"),
            (.infinite, false, "unbounded viewport"),
            (nil, false, "no containing scroll view")
        ]
        for (bounds, expected, description) in visible {
            try check(ArtistBannerLighting.isVisible(size: size, scrollBounds: bounds) == expected,
                      "Artist banner display-link admission: \(description)")
        }
    }
}
