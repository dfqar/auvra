import AppKit
import SwiftUI

enum AppMotion {
    static func navigation(_ reduced: Bool) -> Animation? { reduced ? nil : .easeInOut(duration: 0.3) }
    static func listening(_ reduced: Bool) -> Animation? { reduced ? nil : .easeInOut(duration: 0.42) }
    static func feedback(_ reduced: Bool) -> Animation? { reduced ? nil : .easeInOut(duration: 0.18) }
    static func cover(_ reduced: Bool) -> Animation? { reduced ? nil : .smooth(duration: 0.52) }
    static func playbackMood(_ reduced: Bool) -> Animation? { reduced ? nil : .easeInOut(duration: 0.36) }
}

enum AppAppearance: String, CaseIterable, Identifiable {
    case system, light, dark
    static let preferenceKey = "AuvraAppearance"
    var id: String { rawValue }
    var title: String { switch self { case .system: "System"; case .light: "Light"; case .dark: "Dark" } }
    var colorScheme: ColorScheme? { switch self { case .system: nil; case .light: .light; case .dark: .dark } }
    var nativeAppearance: NSAppearance? {
        switch self { case .system: nil; case .light: NSAppearance(named: .aqua); case .dark: NSAppearance(named: .darkAqua) }
    }
    static var current: Self { Self(rawValue: UserDefaults.standard.string(forKey: preferenceKey) ?? "") ?? .system }
    @MainActor func apply() { NSApp.appearance = nativeAppearance }
}

private struct AppAppearanceModifier: ViewModifier {
    @AppStorage(AppAppearance.preferenceKey) private var appearance = AppAppearance.system
    func body(content: Content) -> some View {
        content.preferredColorScheme(appearance.colorScheme).tint(AuvraColors.accent)
            .onAppear { appearance.apply() }
            .onChange(of: appearance) { appearance.apply() }
    }
}
extension View {
    func auvraAppearance() -> some View { modifier(AppAppearanceModifier()) }
}

struct WindowGlass: DynamicProperty {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.windowGlassEnabled) private var enabled

    var isEnabled: Bool {
        if #available(macOS 26, *) { return enabled && !reduceTransparency }
        return false
    }

    @ViewBuilder func surface<S: Shape, Content: View>(_ shape: S, content: Content) -> some View {
        if #available(macOS 26, *), isEnabled {
            content.glassEffect(.clear, in: shape)
        } else {
            content
        }
    }

    @ViewBuilder func playerSurface<S: Shape, Content: View>(_ shape: S, enabled: Bool, content: Content) -> some View {
        if #available(macOS 26, *), isEnabled {
            content.background {
                // Soften native regular glass while keeping Frost visible beneath it.
                Color.clear
                    .glassEffect(enabled ? .regular : .identity, in: shape)
                    .opacity(0.84)
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
            }
        } else {
            content
        }
    }

    @MainActor @ViewBuilder func container<Content: View>(spacing: CGFloat, @ViewBuilder content: () -> Content) -> some View {
        if #available(macOS 26, *), isEnabled {
            GlassEffectContainer(spacing: spacing, content: content)
        } else {
            content()
        }
    }
}

extension EnvironmentValues {
    @Entry var windowGlassEnabled = true
}

/// Shared semantic colors resolve in SwiftUI and native sheets/panels alike.
enum AuvraColors {
    private static func rgb(_ r: Double, _ g: Double, _ b: Double, _ a: Double = 1) -> NSColor {
        NSColor(srgbRed: r, green: g, blue: b, alpha: a)
    }
    private static func adaptive(_ name: String, light: NSColor, dark: NSColor) -> Color {
        Color(nsColor: NSColor(name: NSColor.Name("Auvra." + name)) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
        })
    }
    static let background = adaptive("background", light: rgb(0.976, 0.98, 0.965), dark: rgb(0.08, 0.08, 0.08))
    static let sidebar = adaptive("sidebar", light: rgb(0.928, 0.941, 0.918), dark: rgb(0.105, 0.11, 0.105))
    static let sidebarHeading = adaptive("sidebarHeading", light: rgb(0.34, 0.37, 0.33), dark: rgb(0.64, 0.66, 0.64))
    static let playerBar = adaptive("playerBar", light: rgb(0.947, 0.957, 0.937), dark: rgb(0.065, 0.065, 0.065))
    static let raised = adaptive("raised", light: .white, dark: rgb(0.12, 0.12, 0.12))
    static let card = adaptive("card", light: rgb(0.925, 0.945, 0.929), dark: .white.withAlphaComponent(0.035))
    static let cardEmphasis = adaptive("cardEmphasis", light: rgb(0.89, 0.925, 0.9), dark: .white.withAlphaComponent(0.06))
    static let subtle = adaptive("subtle", light: .black.withAlphaComponent(0.045), dark: .white.withAlphaComponent(0.045))
    static let hover = adaptive("hover", light: .black.withAlphaComponent(0.035), dark: .white.withAlphaComponent(0.035))
    static let selection = adaptive("selection", light: rgb(0.06, 0.46, 0.43, 0.1), dark: .white.withAlphaComponent(0.07))
    static let separator = adaptive("separator", light: .black.withAlphaComponent(0.08), dark: .white.withAlphaComponent(0.04))
    static let accent = adaptive("accent", light: rgb(0.055, 0.45, 0.41), dark: rgb(0.39, 0.86, 0.73))
    static let warning = adaptive("warning", light: rgb(0.59, 0.31, 0.02), dark: .systemOrange)
}
