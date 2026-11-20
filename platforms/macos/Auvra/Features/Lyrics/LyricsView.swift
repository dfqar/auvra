import AppKit
import SwiftUI

struct LyricsToggle: View {
    let player: PlayerStore
    let navigation: AppNavigation?
    var body: some View {
        Button { navigation?.showLyrics(player: player) } label: {
            Image(systemName: "quote.bubble").foregroundStyle(player.lyrics.isVisible ? AuvraColors.accent : Color.secondary)
                .playerButtonTarget()
        }.buttonStyle(.plain).disabled(player.current == nil)
            .accessibilityLabel(player.lyrics.isVisible ? "Hide lyrics" : "Show lyrics")
            .accessibilityValue(player.lyrics.isVisible ? "On" : "Off").help("Lyrics · ⇧⌘L")
    }
}

struct LyricsTranslationToggle: View {
    let lyrics: LyricsStore
    @Environment(\.locale) private var locale
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    static func isAvailable(for locale: Locale) -> Bool {
        if #available(macOS 15, *) { return true }
        return OfflineLyricsTranslation.hasPack(target: locale.language)
    }

    private var isTranslating: Bool { lyrics.translationRequest != nil }
    private var targetName: String {
        let identifier = locale.language.minimalIdentifier
        return locale.localizedString(forIdentifier: identifier) ?? identifier
    }
    private var actionTitle: String {
        if isTranslating { return String(localized: "Cancel translation") }
        if lyrics.showsTranslation { return String(localized: "Show original lyrics") }
        return String(localized: "Translate lyrics into \(targetName)")
    }

    var body: some View {
        Button { lyrics.toggleTranslation(to: lyrics.activeTranslationLanguage ?? locale.language) } label: {
            Group {
                if isTranslating { ProgressView().controlSize(.mini).accessibilityHidden(true) }
                else { Image(systemName: "translate").foregroundStyle(lyrics.showsTranslation ? AuvraColors.accent : Color.secondary) }
            }.playerButtonTarget()
        }.buttonStyle(.plain)
            .accessibilityLabel(actionTitle)
            .accessibilityValue(isTranslating ? String(localized: "Translating") : lyrics.showsTranslation ? String(localized: "Translated") : "")
            .accessibilityIdentifier("translate-lyrics")
            .help(actionTitle)
            .animation(AppMotion.feedback(reduceMotion), value: isTranslating)
    }
}

struct LyricsView: View {
    let player: PlayerStore
    let lyrics: LyricsStore
    @Environment(\.locale) private var locale
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            if !player.isChangingTrack, let document = lyrics.displayedDocument, !document.instrumental {
                LyricsScrollingText(player: player, lyrics: lyrics, document: document)
                    .id(lyrics.selection?.id).transition(.opacity)
                if lyrics.canTranslate, !LyricsTranslationToggle.isAvailable(for: locale) {
                    Text("Lyrics translation on macOS 14 requires the offline pack for your language.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            } else {
                placeholder
            }
        }
            .padding(.horizontal, 12).padding(.vertical, 14)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
            .overlay(alignment: .bottomLeading) { notice }
            .systemLyricsTranslation(lyrics)
            .animation(AppMotion.navigation(reduceMotion), value: lyrics.presentationRevision)
            .animation(AppMotion.navigation(reduceMotion), value: lyrics.showsTranslation)
            .animation(AppMotion.feedback(reduceMotion), value: lyrics.isLoading)
            .animation(AppMotion.feedback(reduceMotion), value: lyrics.issue)
    }

    private var placeholder: some View {
        Group {
            if player.isChangingTrack {
                ProgressView("Switching tracks…").controlSize(.small)
            } else if lyrics.isLoading {
                ProgressView("Finding lyrics…").controlSize(.small)
                    .accessibilityIdentifier("lyrics-loading")
            } else if let unavailable = lyrics.unavailability {
                Text(unavailable.message).font(.callout).foregroundStyle(.secondary)
                    .accessibilityIdentifier("lyrics-unavailable")
                    .task(id: unavailable) {
                        do { try await Task.sleep(for: .milliseconds(1500)) }
                        catch { return }
                        guard !Task.isCancelled, !player.isChangingTrack else { return }
                        lyrics.dismissUnavailable(unavailable)
                    }
            } else if let issue = lyrics.issue {
                Text(issue).font(.callout).foregroundStyle(.secondary)
            } else {
                Text(lyrics.selection == nil ? "Choose a track to see its lyrics." : "Lyrics will appear here.")
                    .font(.title3).foregroundStyle(.secondary)
            }
        }
        .multilineTextAlignment(.center)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
    }

    private var notice: some View {
        Group {
            if !player.isChangingTrack, let notice = lyrics.notice {
                Text(notice)
                    .font(.callout).foregroundStyle(.white.opacity(0.94))
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 14).padding(.vertical, 9)
                    .background(reduceTransparency ? Color(white: 0.14) : Color.black.opacity(0.72), in: .rect(cornerRadius: 14))
                    .overlay { RoundedRectangle(cornerRadius: 14).strokeBorder(.white.opacity(0.14)) }
                    .padding(14)
                    .accessibilityIdentifier("lyrics-translation-status")
                    .transition(reduceMotion ? .identity : .asymmetric(
                        insertion: .opacity.animation(.easeOut(duration: 0.14)),
                        removal: .opacity.animation(.easeInOut(duration: 0.24))
                    ))
            }
        }
        .allowsHitTesting(false)
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.20), value: lyrics.notice)
    }
}

private struct LyricsScrollingText: View {
    private struct CenterTarget: Equatable {
        let index: Int
        var seekRequest: UInt64?
    }

    private struct FollowAnchor {
        let index: Int
        let milliseconds: Double
        let uptime: TimeInterval
    }

    private struct LineSeek {
        let index: Int
        let start: UInt64
        let position: UInt64
        let request: UInt64
    }

    let player: PlayerStore
    let lyrics: LyricsStore
    let document: LyricsDocument
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.scenePhase) private var scenePhase
    @State private var lastAnchor: FollowAnchor?
    @State private var didCenter = false
    @State private var ripples: [LyricsRowRipple] = []
    @State private var lineSeek: LineSeek?
    @State private var lastManualScroll: ContinuousClock.Instant?
    @GestureState private var isDragging = false

    private var playsInForeground: Bool { player.isPlaying && scenePhase == .active }
    private var updatesLyrics: Bool { document.hasTiming && playsInForeground }
    private var frameInterval: TimeInterval {
        if !ripples.isEmpty { return 1.0 / 60 }
        return reduceMotion ? 0.1 : 1.0 / 30
    }

    var body: some View {
        let visibleIndices = document.lines.indices.filter { index in
            let line = document.lines[index]
            return line.hasText || line.backingLines.contains(where: \.hasText)
        }
        GeometryReader { geometry in
            let fontSize = min(46, max(34, (geometry.size.height * 0.07).rounded()))
            ScrollViewReader { proxy in
                TimelineView(.animation(minimumInterval: frameInterval, paused: !updatesLyrics && ripples.isEmpty)) { _ in
                    let position = player.positionMilliseconds()
                    let clock = LyricsPresentationClock.time(position: position, correction: lyrics.offsetMilliseconds, playing: player.isPlaying)
                    let seek = lineSeek
                    let milliseconds = seek.map { Double($0.start) } ?? clock
                    let paused = player.snapshot?.state == .paused
                    let playbackFocus = document.focusedLine(at: clock, paused: paused)
                    let focused = seek?.index ?? playbackFocus
                    let anchor = focused ?? (updatesLyrics ? document.upcomingLine(at: milliseconds) : nil)
                        ?? document.precedingLine(at: milliseconds)
                    let target = lyrics.followsPlayback ? anchor.map { CenterTarget(index: $0, seekRequest: seek?.request) } : nil
                    let now = ProcessInfo.processInfo.systemUptime
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: LyricsRowRipple.rowSpacing) {
                            ForEach(visibleIndices, id: \.self) { index in
                                lyricRow(document.lines[index], index: index, milliseconds: milliseconds, focused: focused,
                                         anchor: anchor, paused: paused, fontSize: fontSize)
                                    .offset(y: LyricsRowRipple.offset(for: index, at: now, ripples: ripples))
                                    .id(index)
                            }
                            attribution
                        }
                        .padding(.vertical, document.hasTiming ? geometry.size.height / 2 : 16)
                    }
                    .scrollIndicators(.hidden)
                    .background { LyricsScrollObserver(onScroll: userScrolled) }
                    .simultaneousGesture(DragGesture(minimumDistance: 4)
                        .updating($isDragging) { _, dragging, _ in dragging = true }
                        .onChanged { _ in userScrolled() })
                    .task(id: isDragging ? nil : lastManualScroll) {
                        guard let lastManualScroll, !isDragging else { return }
                        do { try await ContinuousClock().sleep(until: lastManualScroll.advanced(by: .seconds(2))) }
                        catch { return }
                        guard self.lastManualScroll == lastManualScroll, !isDragging else { return }
                        resumeFollowing()
                    }
                    .onChange(of: isDragging) { _, dragging in
                        if !dragging, lastManualScroll != nil { lastManualScroll = .now }
                    }
                    .onChange(of: target, initial: true) { old, new in
                        guard let new else { lastAnchor = nil; return }
                        if old?.index != new.index || new.seekRequest != nil || lastAnchor == nil {
                            center(new.index, milliseconds: milliseconds, proxy: proxy, natural: new.seekRequest == nil)
                        }
                    }
                    .onChange(of: lyrics.showsTranslation) { _, _ in
                        if let target { center(target.index, milliseconds: milliseconds, proxy: proxy, natural: false) }
                    }
                    .onChange(of: lyrics.followsPlayback) { _, follows in
                        if follows { lastManualScroll = nil }
                    }
                    .onChange(of: lyrics.presentationRevision) { _, _ in lastAnchor = nil; lineSeek = nil }
                    .onChange(of: lyrics.offsetSeconds) { _, _ in lastAnchor = nil; lineSeek = nil }
                    .onChange(of: reduceMotion) { _, _ in lastAnchor = nil; ripples = [] }
                    .onChange(of: scenePhase) { _, _ in lastAnchor = nil; ripples = [] }
                    .onChange(of: player.isPlaying) { _, _ in lastAnchor = nil }
                    .onChange(of: resolvedSeek(seek, position: position, focus: playbackFocus), initial: true) { _, resolved in
                        if let resolved, lineSeek?.request == resolved { lineSeek = nil }
                    }
                    .task(id: seek?.request) {
                        guard let seek else { return }
                        do { try await Task.sleep(for: .seconds(2)) }
                        catch { return }
                        if lineSeek?.request == seek.request { lineSeek = nil }
                    }
                    .task(id: ripples.last?.startedAt) {
                        guard let startedAt = ripples.last?.startedAt else { return }
                        do { try await Task.sleep(for: .seconds(LyricsRowRipple.duration)) }
                        catch { return }
                        if ripples.last?.startedAt == startedAt { ripples = [] }
                    }
                    .mask {
                        LinearGradient(stops: [.init(color: .clear, location: 0), .init(color: .black, location: 0.12),
                                               .init(color: .black, location: 0.86), .init(color: .clear, location: 1)],
                                       startPoint: .top, endPoint: .bottom)
                    }
                }
            }
        }
        .onDisappear {
            if lastManualScroll != nil { resumeFollowing() }
        }
    }

    @ViewBuilder
    private func lyricRow(_ line: LyricsLine, index: Int, milliseconds: Double, focused: Int?, anchor: Int?,
                          paused: Bool, fontSize: CGFloat) -> some View {
        let distance = anchor.map { abs($0 - index) } ?? 2
        let blur = document.hasTiming && lyrics.followsPlayback && !reduceMotion && !reduceTransparency && distance > 0
            ? min(2.7, 1.1 + CGFloat(distance - 1) * 0.4) : 0
        let row = LyricsVocalGroupView(line: line, milliseconds: milliseconds, timed: document.hasTiming,
                                       reduceMotion: reduceMotion, focused: focused == index, paused: paused, baseFontSize: fontSize)
            .blur(radius: blur)
            .animation(AppMotion.feedback(reduceMotion), value: blur)
        let label = line.voicesWithText.map(\.text).joined(separator: "\n")
        if let start = line.seekStartMs {
            Button {
                let position = lyrics.playbackPosition(forLineStart: start)
                guard let request = player.seek(milliseconds: position) else { return }
                lineSeek = LineSeek(index: index, start: start, position: position, request: request)
                resumeFollowing()
            } label: { row }
            .buttonStyle(.plain).disabled(!player.canSeek)
            .accessibilityLabel(label).accessibilityHint("Jump to this line")
        } else {
            row.accessibilityElement(children: .ignore).accessibilityLabel(label)
        }
    }

    @ViewBuilder
    private var attribution: some View {
        if let result = lyrics.result, let url = result.sourceURL, url.scheme == "https",
           (result.source == UnisonLyrics.source && url.host == "unison.boidu.dev")
           || (result.source == LrcRedLyrics.source && url.host == "lrc.red")
           || (result.source == MusixmatchLyrics.source && url.host == "www.musixmatch.com") {
            Link("Lyrics from \(result.source)", destination: url)
                .font(.caption).foregroundStyle(.secondary)
                .accessibilityIdentifier("lyrics-source-attribution")
        }
    }

    private func userScrolled() {
        guard document.hasTiming, lyrics.followsPlayback || lastManualScroll != nil else { return }
        lastManualScroll = .now
        lineSeek = nil
        lastAnchor = nil
        lyrics.followsPlayback = false
    }

    private func resumeFollowing() {
        lastManualScroll = nil
        lyrics.followsPlayback = true
    }

    private func resolvedSeek(_ seek: LineSeek?, position: Double, focus: Int?) -> UInt64? {
        guard let seek, !player.pendingCommands.contains(seek.request) else { return nil }
        if player.lastCommandOutcome?.requestId == seek.request, player.lastCommandOutcome?.error != nil { return seek.request }
        let tolerance: Double = player.isPlaying ? 500 : 2
        return abs(position - Double(seek.position)) <= tolerance && focus == seek.index ? seek.request : nil
    }

    private func center(_ index: Int, milliseconds: Double, proxy: ScrollViewProxy, natural: Bool) {
        let now = ProcessInfo.processInfo.systemUptime
        let previous = lastAnchor
        lastAnchor = FollowAnchor(index: index, milliseconds: milliseconds, uptime: now)
        let rate = player.snapshot?.playbackRate ?? 1
        let followsCue = natural && updatesLyrics && !reduceMotion && rate.isFinite && rate > 0 && previous.map { anchor in
            index == anchor.index + 1 && milliseconds > anchor.milliseconds
                && abs(milliseconds - anchor.milliseconds - (now - anchor.uptime) * rate * 1000) < 280
        } == true
        if followsCue {
            ripples.removeAll { now - $0.startedAt >= LyricsRowRipple.duration }
            ripples.append(LyricsRowRipple(target: index, startedAt: now))
        }
        let animation: Animation? = reduceMotion || !didCenter ? nil : .smooth(duration: followsCue ? 0.56 : 0.40)
        didCenter = true
        withAnimation(animation) { proxy.scrollTo(index, anchor: .center) }
    }
}

enum LyricsPresentationClock {
    static let playbackDelayMilliseconds = 75.0

    static func time(position: Double, correction: Double, playing: Bool) -> Double {
        position + correction - (playing ? playbackDelayMilliseconds : 0)
    }
}

struct LyricsRowRipple {
    static let rowSpacing: CGFloat = 12
    static let duration = 0.60
    let target: Int
    let startedAt: TimeInterval

    static func offset(for index: Int, at uptime: TimeInterval, ripples: [Self]) -> CGFloat {
        let lag = ripples.reduce(0.0) { accumulated, ripple in
            let stage = min(4, max(0, index - ripple.target + 2))
            let elapsed = uptime - ripple.startedAt
            func progress(_ time: Double) -> Double {
                let t = min(1, max(0, time / 0.44))
                return t * t * (3 - 2 * t)
            }
            return accumulated + progress(elapsed) - progress(elapsed - Double(stage) * 0.04)
        }
        // The native stack owns travel. Even several interrupted ripples can never
        // consume the entire gap, including at the edge where lazy rows appear.
        return (rowSpacing - 4) * CGFloat(tanh(3 * max(0, lag)))
    }
}

extension LyricsDocument {
    var firstVisibleLine: Int? { lines.firstIndex { $0.hasText || $0.backingLines.contains(where: \.hasText) } }

    func precedingLine(at milliseconds: Double) -> Int? {
        guard hasTiming, milliseconds.isFinite else { return nil }
        return lines.lastIndex { line in
            line.voicesWithText.contains { voice in voice.startMs.map { Double($0) <= milliseconds } == true }
        } ?? firstVisibleLine
    }

    func upcomingLine(at milliseconds: Double) -> Int? {
        guard hasTiming, milliseconds.isFinite else { return nil }
        return lines.indices.first { index in
            let line = lines[index]
            guard let start = line.seekStartMs else { return false }
            let ahead = Double(start) - milliseconds
            return ahead > 0 && ahead <= 1_100 && !line.voicesWithText.isEmpty
        }
    }
}

struct LyricsVocalGroupView: View {
    let line: LyricsLine
    let milliseconds: Double
    let timed, reduceMotion: Bool
    var focused = true
    var paused = false
    var baseFontSize: CGFloat = 28

    private func isActive(_ voice: LyricsLine, holdsCompletedWords: Bool) -> Bool {
        focused && (voice.contains(milliseconds) || (paused && voice.contains(milliseconds + 1))
                    || (holdsCompletedWords && voice.hasCompletedWords(at: milliseconds)))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            if line.hasText {
                LyricsLineView(line: line, milliseconds: milliseconds, active: isActive(line, holdsCompletedWords: true), timed: timed,
                               reduceMotion: reduceMotion, fontSize: baseFontSize)
            }
            ForEach(Array(line.backingLines.enumerated()), id: \.offset) { _, voice in
                if voice.hasText {
                    LyricsLineView(line: voice, milliseconds: milliseconds, active: isActive(voice, holdsCompletedWords: !line.hasText),
                                   timed: timed, reduceMotion: reduceMotion, fontSize: (baseFontSize * 0.68).rounded())
                        .padding(.leading, line.hasText ? 12 : 0)
                }
            }
        }.frame(maxWidth: .infinity, alignment: .leading).contentShape(Rectangle())
    }
}

struct LyricsLineView: View {
    fileprivate static let horizontalGutter: CGFloat = 10
    fileprivate static let verticalGutter: CGFloat = 7
    let line: LyricsLine
    let milliseconds: Double
    let active, timed, reduceMotion: Bool
    var fontSize: CGFloat = 28
    private var phase: LyricsPaintPhase {
        if !timed { return .sung }
        guard active else { return .unsung }
        return line.words.isEmpty || line.hasCompletedWords(at: milliseconds) ? .sung : .sweeping
    }

    var body: some View {
        LyricsLineCanvas(line: line, milliseconds: milliseconds, phase: phase,
                         reduceMotion: reduceMotion, fontSize: fontSize)
            .accessibilityHidden(true).transition(.opacity)
    }
}

enum LyricsPaintPhase: Equatable { case unsung, sweeping, sung }

private enum LyricsInk {
    static func dimmed(_ scheme: ColorScheme) -> NSColor {
        let gray: CGFloat = scheme == .light ? 0.44 : 0.54
        return NSColor(srgbRed: gray, green: gray, blue: gray, alpha: 1)
    }
    static func unsung(_ scheme: ColorScheme) -> NSColor {
        let gray: CGFloat = scheme == .light ? 0.40 : 0.66
        return NSColor(srgbRed: gray, green: gray, blue: gray, alpha: 1)
    }
    static func sung(_ scheme: ColorScheme) -> NSColor {
        scheme == .light ? NSColor(srgbRed: 0.08, green: 0.1, blue: 0.09, alpha: 1) : .white
    }
    static func glass(_ scheme: ColorScheme, space: CGColorSpace) -> CGGradient? {
        let colors = scheme == .light
            ? [NSColor(srgbRed: 0.10, green: 0.15, blue: 0.19, alpha: 1), .black,
               NSColor(srgbRed: 0.20, green: 0.24, blue: 0.29, alpha: 1), .black]
            : [NSColor(srgbRed: 0.89, green: 0.95, blue: 1, alpha: 0.96), .white,
               NSColor(srgbRed: 0.83, green: 0.90, blue: 0.97, alpha: 0.96), .white]
        return CGGradient(colorsSpace: space, colors: colors.map(\.cgColor) as CFArray, locations: [0, 0.18, 0.70, 1])
    }
}

/// Shapes the entire source line once, preserving bidi, ligatures and graphemes.
/// Masks consume real word intervals; glyph widths only control the visual sweep.
@MainActor
final class LyricsTextLayout: NSObject, NSLayoutManagerDelegate {
    struct Segment {
        let rect: CGRect
        let rightToLeft: Bool
    }
    struct WordShape {
        let glyphs: NSRange
        let segments: [Segment]
        let intervals: [Range<UInt64>]
        var durationMs: UInt64 { intervals.reduce(0) { $0 + ($1.upperBound - $1.lowerBound) } }
        func progress(at milliseconds: Double) -> Double {
            if intervals.count == 1, let interval = intervals.first {
                return LyricsWord.progress(at: milliseconds, startMs: interval.lowerBound, endMs: interval.upperBound)
            }
            let duration = durationMs
            guard duration > 0 else { return 0 }
            let elapsed = intervals.reduce(0.0) { total, interval in
                let progress = LyricsWord.progress(at: milliseconds, startMs: interval.lowerBound, endMs: interval.upperBound)
                return total + progress * Double(interval.upperBound - interval.lowerBound)
            }
            return elapsed / Double(duration)
        }
        func revealed(at progress: Double) -> [CGRect] {
            guard progress.isFinite, progress > 0 else { return [] }
            var remaining = segments.reduce(CGFloat(0)) { $0 + $1.rect.width } * min(1, progress)
            var result: [CGRect] = []
            for segment in segments where remaining > 0 {
                let width = min(segment.rect.width, remaining)
                let x = segment.rightToLeft ? segment.rect.maxX - width : segment.rect.minX
                result.append(CGRect(x: x, y: segment.rect.minY, width: width, height: segment.rect.height))
                remaining -= width
            }
            return result
        }
    }
    private let storage = NSTextStorage()
    private let manager = NSLayoutManager()
    private let container = NSTextContainer(containerSize: .zero)
    private var line: LyricsLine?
    private var fontSize: CGFloat = 28
    private var width: CGFloat?
    private var unsungInk = LyricsInk.unsung(.dark)
    private var dimmedInk = LyricsInk.dimmed(.dark)
    private var sungInk = LyricsInk.sung(.dark)
    private var inkScheme = ColorScheme.dark
    private var glassInk: (image: CGImage, scale: CGFloat)?
    private(set) var words: [WordShape] = []
    private(set) var size = CGSize.zero
    private(set) var layoutGeneration = 0
    override init() {
        super.init()
        storage.addLayoutManager(manager); manager.addTextContainer(container)
        manager.delegate = self
        container.lineFragmentPadding = 0
    }

    // Use the same color-only overlay for visible and offscreen drawing.
    nonisolated func layoutManager(_ layoutManager: NSLayoutManager,
                                   shouldUseTemporaryAttributes attrs: [NSAttributedString.Key: Any],
                                   forDrawingToScreen toScreen: Bool, atCharacterIndex charIndex: Int,
                                   effectiveRange: NSRangePointer?) -> [NSAttributedString.Key: Any]? {
        attrs
    }

    @discardableResult func setLine(_ line: LyricsLine, fontSize: CGFloat = 28) -> Bool {
        guard self.line?.text != line.text || self.line?.words != line.words || self.fontSize != fontSize else { return false }
        self.line = line; self.fontSize = fontSize; width = nil
        let paragraph = NSMutableParagraphStyle()
        paragraph.alignment = .natural; paragraph.baseWritingDirection = .natural
        paragraph.lineBreakMode = .byWordWrapping; paragraph.lineSpacing = 4
        let system = NSFont.systemFont(ofSize: fontSize, weight: .semibold)
        let font = system.fontDescriptor.withDesign(.rounded).flatMap { NSFont(descriptor: $0, size: fontSize) } ?? system
        storage.setAttributedString(NSAttributedString(string: line.text, attributes: [.font: font, .foregroundColor: unsungInk, .paragraphStyle: paragraph]))
        return true
    }
    func setInk(_ scheme: ColorScheme) -> Bool {
        let unsung = LyricsInk.unsung(scheme), sung = LyricsInk.sung(scheme)
        guard unsungInk != unsung || sungInk != sung else { return false }
        unsungInk = unsung; sungInk = sung; dimmedInk = LyricsInk.dimmed(scheme)
        inkScheme = scheme; glassInk = nil
        storage.addAttribute(.foregroundColor, value: unsung, range: NSRange(location: 0, length: storage.length))
        return true
    }
    func measure(width proposed: CGFloat) -> CGSize {
        let nextWidth = proposed.isFinite ? max(1, proposed) : 500
        guard nextWidth != width else { return size }
        width = nextWidth; layoutGeneration += 1; glassInk = nil
        container.containerSize = CGSize(width: nextWidth, height: .greatestFiniteMagnitude)
        manager.ensureLayout(for: container)
        size = CGSize(width: nextWidth, height: ceil(manager.usedRect(for: container).maxY))
        words = []
        guard let line else { return size }
        var levels = [UInt8](repeating: 0, count: manager.numberOfGlyphs)
        manager.getGlyphs(in: NSRange(location: 0, length: levels.count), glyphs: nil, properties: nil, characterIndexes: nil, bidiLevels: &levels)
        // Native shaping can assign the same glyphs to multiple timed spans. Partition
        // those overlaps so only the shared glyphs use the combined cue interval.
        var boundaries: [(index: Int, cue: Int, starts: Bool)] = []
        var offset = 0
        for (cue, word) in line.words.enumerated() {
            let text = word.text as NSString
            let first = text.rangeOfCharacter(from: .whitespacesAndNewlines.inverted)
            let last = text.rangeOfCharacter(from: .whitespacesAndNewlines.inverted, options: .backwards)
            let range = first.location == NSNotFound ? NSRange(location: offset, length: 0) : NSRange(location: offset + first.location, length: NSMaxRange(last) - first.location)
            offset += text.length
            guard range.length > 0, NSMaxRange(range) <= storage.length else { continue }
            let glyphs = manager.glyphRange(forCharacterRange: range, actualCharacterRange: nil)
            guard glyphs.length > 0, glyphs.location != NSNotFound, NSMaxRange(glyphs) <= levels.count else { continue }
            boundaries.append((glyphs.location, cue, true))
            boundaries.append((NSMaxRange(glyphs), cue, false))
        }
        boundaries.sort { $0.index < $1.index }
        var active = Set<Int>()
        var last = 0
        for boundary in boundaries {
            if boundary.index > last, !active.isEmpty {
                let glyphs = NSRange(location: last, length: boundary.index - last)
                let intervals = cueIntervals(active, words: line.words)
                words.append(WordShape(glyphs: glyphs, segments: segments(for: glyphs, levels: levels), intervals: intervals))
            }
            last = boundary.index
            if boundary.starts { active.insert(boundary.cue) }
            else { active.remove(boundary.cue) }
        }
        return size
    }

    private func cueIntervals(_ cues: Set<Int>, words: [LyricsWord]) -> [Range<UInt64>] {
        var intervals: [Range<UInt64>] = []
        for cue in cues.sorted(by: { words[$0].startMs < words[$1].startMs }) {
            let word = words[cue]
            guard word.endMs > word.startMs else { continue }
            if let last = intervals.last, word.startMs <= last.upperBound {
                intervals[intervals.count - 1] = last.lowerBound..<max(last.upperBound, word.endMs)
            } else {
                intervals.append(word.startMs..<word.endMs)
            }
        }
        return intervals
    }

    private func segments(for glyphs: NSRange, levels: [UInt8]) -> [Segment] {
        var segments: [Segment] = []
        let text = storage.mutableString
        var start = glyphs.location
        while start < NSMaxRange(glyphs) {
            // Selection rect enumeration may merge consecutive full rows.
            // Split at line fragments first so a sweep visits one row at a time.
            var fragment = NSRange()
            _ = manager.lineFragmentRect(forGlyphAt: start, effectiveRange: &fragment)
            let limit = min(NSMaxRange(glyphs), NSMaxRange(fragment))
            let rtl = levels[start] % 2 == 1
            var end = start + 1
            while end < limit, (levels[end] % 2 == 1) == rtl { end += 1 }
            // TextKit expands a selected newline to the margin. The break lays out
            // the next row but must not consume any of the timed sweep.
            var visibleEnd = end
            while visibleEnd > start {
                let character = manager.characterIndexForGlyph(at: visibleEnd - 1)
                guard character < text.length, let scalar = UnicodeScalar(text.character(at: character)),
                      CharacterSet.newlines.contains(scalar) else { break }
                visibleEnd -= 1
            }
            var rects: [CGRect] = []
            if visibleEnd > start {
                manager.enumerateEnclosingRects(forGlyphRange: NSRange(location: start, length: visibleEnd - start), withinSelectedGlyphRange: NSRange(location: NSNotFound, length: 0), in: container) { rect, _ in
                    if rect.width > 0 { rects.append(rect) }
                }
            }
            rects.sort { a, b in a.minY == b.minY ? (rtl ? a.minX > b.minX : a.minX < b.minX) : a.minY < b.minY }
            segments += rects.map { Segment(rect: $0, rightToLeft: rtl) }
            start = end
        }
        return segments
    }

    func draw(milliseconds: Double, phase: LyricsPaintPhase, reduceMotion: Bool, reduceTransparency: Bool = false) {
        guard line != nil, let context = NSGraphicsContext.current?.cgContext else { return }
        let all = NSRange(location: 0, length: manager.numberOfGlyphs)
        let characters = NSRange(location: 0, length: storage.length)
        if phase == .unsung {
            glassInk = nil
            manager.addTemporaryAttribute(.foregroundColor, value: dimmedInk, forCharacterRange: characters)
            manager.drawGlyphs(forGlyphRange: all, at: .zero)
            manager.removeTemporaryAttribute(.foregroundColor, forCharacterRange: characters)
            return
        }

        let highlights = phase == .sweeping ? words.map { ($0, $0.progress(at: milliseconds)) } : []
        if phase == .sweeping {
            manager.drawGlyphs(forGlyphRange: all, at: .zero)
            if !reduceMotion && !reduceTransparency {
                for (word, progress) in highlights {
                    let glow = glowStrength(for: word, progress: progress)
                    guard glow > 0 else { continue }
                    context.saveGState()
                    context.setShadow(offset: .zero, blur: 3 + 4 * glow,
                                      color: sungInk.withAlphaComponent(0.31 * glow).cgColor)
                    manager.drawGlyphs(forGlyphRange: word.glyphs, at: .zero)
                    context.restoreGState()
                }
            }
        }
        manager.addTemporaryAttribute(.foregroundColor, value: sungInk, forCharacterRange: characters)
        defer { manager.removeTemporaryAttribute(.foregroundColor, forCharacterRange: characters) }
        let sheen = reduceTransparency ? nil : glassImage(for: all, in: context)
        if phase == .sung {
            if let sheen { drawGlass(sheen, in: context) }
            else { manager.drawGlyphs(forGlyphRange: all, at: .zero) }
            return
        }

        for (word, progress) in highlights {
            if progress >= 1 {
                // Selection rectangles describe advances, which can omit glyph overhangs.
                manager.drawGlyphs(forGlyphRange: word.glyphs, at: .zero)
                continue
            }
            var rects = word.revealed(at: progress)
            guard var edge = rects.popLast() else { continue }
            let segment = word.segments[rects.count]
            // The feather follows the sung edge and disappears as it reaches
            // the final advance. Earlier wrapped fragments stay fully filled.
            let remaining = segment.rect.width - edge.width
            let feather = min(6, edge.width, max(0, remaining))
            let featherStart = segment.rightToLeft ? edge.minX : edge.maxX - feather
            if segment.rightToLeft { edge.origin.x += feather }
            edge.size.width -= feather
            rects.append(edge)
            func fill(_ clips: [CGRect], opacity: CGFloat = 1) {
                context.saveGState()
                context.clip(to: clips)
                context.setAlpha(opacity)
                if let sheen { drawGlass(sheen, in: context) }
                else { manager.drawGlyphs(forGlyphRange: word.glyphs, at: .zero) }
                context.restoreGState()
            }
            fill(rects)
            if feather > 0 {
                let strip = feather / 4
                for index in 0..<4 {
                    let opacity = (3.5 - CGFloat(index)) / 4
                    let offset = segment.rightToLeft ? 3 - index : index
                    fill([CGRect(x: featherStart + CGFloat(offset) * strip, y: edge.minY,
                                 width: strip, height: edge.height)], opacity: opacity)
                }
            }
        }
    }

    private func glowStrength(for word: WordShape, progress: Double) -> CGFloat {
        guard word.intervals.count == 1, word.segments.count == 1,
              word.durationMs >= 1000, progress > 0, progress < 1 else { return 0 }
        func ease(_ value: Double) -> Double {
            let t = min(1, max(0, value))
            return t * t * (3 - 2 * t)
        }
        let strength = min(1, (Double(word.durationMs) - 850) / 1600)
        return CGFloat(strength * ease(progress / 0.62) * ease((1 - progress) / 0.28))
    }

    /// Cache the sheen over native glyph alpha, including fallback fonts and
    /// ligatures. No background material or separate glass views per word.
    private func glassImage(for glyphs: NSRange, in context: CGContext) -> (image: CGImage, scale: CGFloat)? {
        let device = context.convertToDeviceSpace(CGSize(width: 1, height: 1))
        let scale = max(1, min(2, abs(device.width)))
        if let glassInk, glassInk.scale == scale { return glassInk }
        let usedWidth = min(size.width, manager.usedRect(for: container).maxX + 2)
        let width = ceil(usedWidth * scale), height = ceil(size.height * scale)
        guard width > 0, height > 0, width * height <= 1_500_000,
              let space = CGColorSpace(name: CGColorSpace.sRGB),
              let gradient = LyricsInk.glass(inkScheme, space: space),
              let bitmap = CGContext(data: nil, width: Int(width), height: Int(height), bitsPerComponent: 8,
                                     bytesPerRow: 0, space: space,
                                     bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        bitmap.translateBy(x: 0, y: height)
        bitmap.scaleBy(x: scale, y: -scale)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: bitmap, flipped: true)
        manager.drawGlyphs(forGlyphRange: glyphs, at: .zero)
        NSGraphicsContext.restoreGraphicsState()

        bitmap.setBlendMode(.sourceIn)
        manager.enumerateLineFragments(forGlyphRange: glyphs) { rect, _, _, range, _ in
            let baseline = rect.minY + self.manager.location(forGlyphAt: range.location).y
            bitmap.saveGState()
            bitmap.clip(to: CGRect(x: 0, y: rect.minY, width: self.size.width, height: rect.height))
            bitmap.drawLinearGradient(gradient,
                                      start: CGPoint(x: 0, y: baseline - self.fontSize * 0.76),
                                      end: CGPoint(x: 0, y: baseline + self.fontSize * 0.12),
                                      options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
            bitmap.restoreGState()
        }
        guard let image = bitmap.makeImage() else { return nil }
        let ink = (image, scale)
        glassInk = ink
        return ink
    }

    private func drawGlass(_ ink: (image: CGImage, scale: CGFloat), in context: CGContext) {
        let width = CGFloat(ink.image.width) / ink.scale, height = CGFloat(ink.image.height) / ink.scale
        context.saveGState()
        context.translateBy(x: 0, y: height)
        context.scaleBy(x: 1, y: -1)
        context.draw(ink.image, in: CGRect(x: 0, y: 0, width: width, height: height))
        context.restoreGState()
    }
}

private struct LyricsLineCanvas: NSViewRepresentable {
    let line: LyricsLine
    let milliseconds: Double
    let phase: LyricsPaintPhase
    let reduceMotion: Bool
    let fontSize: CGFloat
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    func makeNSView(context: Context) -> LyricsDrawingView { LyricsDrawingView() }
    func updateNSView(_ view: LyricsDrawingView, context: Context) {
        view.update(line, milliseconds: milliseconds, phase: phase, reduceMotion: reduceMotion,
                    reduceTransparency: reduceTransparency, fontSize: fontSize, scheme: colorScheme)
    }
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: LyricsDrawingView, context: Context) -> CGSize? {
        let width = proposal.width ?? 500
        let ink = nsView.renderer.measure(width: width - 2 * LyricsLineView.horizontalGutter)
        return CGSize(width: width, height: ink.height + 2 * LyricsLineView.verticalGutter)
    }
}
private final class LyricsDrawingView: NSView {
    let renderer = LyricsTextLayout()
    private var milliseconds: Double = 0
    private var phase = LyricsPaintPhase.unsung
    private var reduceMotion = false
    private var reduceTransparency = false
    override var isFlipped: Bool { true }
    override func hitTest(_ point: NSPoint) -> NSView? { nil }
    func update(_ line: LyricsLine, milliseconds: Double, phase: LyricsPaintPhase, reduceMotion: Bool,
                reduceTransparency: Bool, fontSize: CGFloat, scheme: ColorScheme) {
        let changed = renderer.setLine(line, fontSize: fontSize)
        let inkChanged = renderer.setInk(scheme)
        if changed || inkChanged || self.phase != phase || self.reduceMotion != reduceMotion
            || self.reduceTransparency != reduceTransparency || (phase == .sweeping && self.milliseconds != milliseconds) { needsDisplay = true }
        self.milliseconds = milliseconds; self.phase = phase; self.reduceMotion = reduceMotion
        self.reduceTransparency = reduceTransparency
    }
    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        _ = renderer.measure(width: bounds.width - 2 * LyricsLineView.horizontalGutter)
        context.saveGState()
        context.translateBy(x: LyricsLineView.horizontalGutter, y: LyricsLineView.verticalGutter)
        renderer.draw(milliseconds: milliseconds, phase: phase, reduceMotion: reduceMotion, reduceTransparency: reduceTransparency)
        context.restoreGState()
    }
}

private struct LyricsScrollObserver: NSViewRepresentable {
    let onScroll: @MainActor () -> Void
    func makeNSView(context: Context) -> LyricsScrollObservationView { LyricsScrollObservationView() }
    func updateNSView(_ view: LyricsScrollObservationView, context: Context) { view.onScroll = onScroll }
    static func dismantleNSView(_ view: LyricsScrollObservationView, coordinator: ()) { view.stop() }
}
/// Observes this scroll region only, returning every event to normal AppKit routing.
final class LyricsScrollObservationView: NSView {
    var onScroll: (@MainActor () -> Void)?
    private var monitor: Any?
    override func hitTest(_ point: NSPoint) -> NSView? { nil }
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow(); stop()
        guard window != nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] event in
            MainActor.assumeIsolated {
                self?.observe(window: event.window, point: event.locationInWindow, deltaX: event.scrollingDeltaX, deltaY: event.scrollingDeltaY)
            }
            return event
        }
    }
    func observe(window eventWindow: NSWindow?, point: NSPoint, deltaX: CGFloat, deltaY: CGFloat) {
        guard let window, window.isVisible, eventWindow === window, !isHiddenOrHasHiddenAncestor,
              deltaX != 0 || deltaY != 0, bounds.intersection(visibleRect).contains(convert(point, from: nil)) else { return }
        onScroll?()
    }
    func stop() { if let monitor { NSEvent.removeMonitor(monitor) }; monitor = nil }
}
