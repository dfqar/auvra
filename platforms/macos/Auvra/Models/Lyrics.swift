import Foundation

struct LyricsWord: Codable, Sendable, Equatable {
    let text: String
    let startMs, endMs: UInt64
    let endEstimated: Bool
    func progress(at milliseconds: Double) -> Double {
        Self.progress(at: milliseconds, startMs: startMs, endMs: endMs)
    }
    static func progress(at milliseconds: Double, startMs: UInt64, endMs: UInt64) -> Double {
        guard milliseconds.isFinite, endMs > startMs else { return 0 }
        return min(1, max(0, (milliseconds - Double(startMs)) / Double(endMs - startMs)))
    }
}
struct LyricsLine: Codable, Sendable, Equatable {
    let text: String
    let startMs, endMs: UInt64?
    let words: [LyricsWord]
    let background: [LyricsLine]?

    init(text: String, startMs: UInt64?, endMs: UInt64?, words: [LyricsWord], background: [LyricsLine]? = nil) {
        self.text = text
        self.startMs = startMs
        self.endMs = endMs
        self.words = words
        self.background = background
    }

    var backingLines: [LyricsLine] { background ?? [] }
    var hasText: Bool { !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    var voicesWithText: [LyricsLine] { ([self] + backingLines).filter(\.hasText) }
    var seekStartMs: UInt64? { ([startMs] + backingLines.map(\.startMs)).compactMap { $0 }.min() }

    func contains(_ milliseconds: Double) -> Bool {
        guard milliseconds.isFinite, let startMs else { return false }
        return milliseconds >= Double(startMs) && endMs.map { milliseconds < Double($0) } != false
    }

    func hasCompletedWords(at milliseconds: Double) -> Bool {
        !words.isEmpty && words.allSatisfy { Double($0.endMs) <= milliseconds }
    }

    fileprivate var isValidVoice: Bool {
        guard text.utf8.count <= 8192, words.count <= 1000 else { return false }
        guard let startMs else { return endMs == nil && words.isEmpty }
        guard startMs <= 86_400_000, endMs.map({ $0 >= startMs && $0 <= 86_400_000 }) != false else { return false }
        return words.isEmpty || (words.map(\.text).joined() == text && words.allSatisfy {
            $0.endMs > $0.startMs && $0.startMs >= startMs && $0.endMs <= (endMs ?? 86_400_000)
        })
    }
}
struct LyricsTranslationLine: Sendable, Equatable {
    let index: Int
    let text: String
    var isBacking = false
}
struct LyricsTranslationRequest: Sendable, Equatable {
    let id: UInt64
    let targetLanguage: Locale.Language
    let lines: [LyricsTranslationLine]
}
enum LyricsTranslationInput {
    case unavailable, tooLong, ready([LyricsTranslationLine])
}
struct LyricsDocument: Codable, Sendable, Equatable {
    let lines: [LyricsLine]
    let instrumental: Bool
    let precision: String
    var isValid: Bool {
        guard ["plain", "line", "word"].contains(precision), lines.count <= 3000 else { return false }
        var cueCount = lines.count
        return lines.allSatisfy { line in
            cueCount += line.backingLines.count
            return cueCount <= 3000 && line.isValidVoice && line.backingLines.count <= 8
                && line.backingLines.allSatisfy { $0.backingLines.isEmpty && $0.isValidVoice }
        }
    }
    var hasTiming: Bool { precision != "plain" && !instrumental }
    var hasTranslatableText: Bool {
        !instrumental && lines.contains { $0.hasText || $0.backingLines.contains(where: \.hasText) }
    }
    func translationInput() -> LyricsTranslationInput {
        guard !instrumental, isValid else { return .unavailable }
        var input: [LyricsTranslationLine] = []
        var totalBytes = 0
        func append(_ line: LyricsLine, index: Int, backing: Bool = false) -> Bool {
            guard line.hasText else { return true }
            let bytes = line.text.utf8.count
            totalBytes += bytes
            guard input.count < 400, bytes <= 4096, totalBytes <= 48 * 1024 else { return false }
            input.append(LyricsTranslationLine(index: index, text: line.text, isBacking: backing))
            return true
        }
        for (index, line) in lines.enumerated() {
            guard append(line, index: index) else { return .tooLong }
        }
        var backingIndex = lines.count
        for line in lines {
            for voice in line.backingLines {
                guard append(voice, index: backingIndex, backing: true) else { return .tooLong }
                backingIndex += 1
            }
        }
        return input.isEmpty ? .unavailable : .ready(input)
    }
    func applyingTranslation(_ translations: [Int: String], to input: [LyricsTranslationLine]) -> LyricsDocument? {
        guard translations.count == input.count, input.allSatisfy({ translations[$0.index] != nil }) else { return nil }
        var totalBytes = 0
        func translate(_ original: LyricsLine, index: Int, background: [LyricsLine]? = nil) -> LyricsLine? {
            let replacement = translations[index]
            let text = replacement ?? original.text
            let bytes = text.utf8.count
            totalBytes += bytes
            guard bytes <= 8192, totalBytes <= 96 * 1024,
                  replacement == nil || !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
            return LyricsLine(text: text, startMs: original.startMs, endMs: original.endMs, words: [], background: background)
        }
        var translated: [LyricsLine] = []
        translated.reserveCapacity(lines.count)
        var backingIndex = lines.count
        for (index, original) in lines.enumerated() {
            var backing: [LyricsLine] = []
            for voice in original.backingLines {
                guard let row = translate(voice, index: backingIndex) else { return nil }
                backing.append(row)
                backingIndex += 1
            }
            guard let row = translate(original, index: index, background: backing.isEmpty ? nil : backing) else { return nil }
            translated.append(row)
        }
        return LyricsDocument(lines: translated, instrumental: instrumental, precision: precision == "word" ? "line" : precision)
    }
    func focusedLine(at milliseconds: Double, paused: Bool = false) -> Int? {
        guard milliseconds.isFinite, hasTiming else { return nil }
        func isActive(_ voice: LyricsLine) -> Bool {
            // A paused output can report the preceding millisecond after an exact seek.
            (voice.contains(milliseconds) || (paused && voice.contains(milliseconds + 1))) && voice.hasText
        }
        var backingFocus: Int?
        for index in lines.indices.reversed() {
            let line = lines[index]
            if isActive(line) { return index }
            if backingFocus == nil, line.backingLines.contains(where: isActive) {
                backingFocus = index
            }
        }
        if let backingFocus { return backingFocus }

        func hasStarted(_ voice: LyricsLine) -> Bool {
            voice.startMs.map { Double($0) <= milliseconds } == true && voice.hasText
        }
        // Between cues, hold the most recent word-timed row only. A newer
        // line-timed row also ends that hold, even after it has gone dim.
        guard let index = lines.lastIndex(where: { hasStarted($0) || $0.backingLines.contains(where: hasStarted) }) else { return nil }
        let line = lines[index]
        if line.hasText { return line.hasCompletedWords(at: milliseconds) ? index : nil }
        return line.backingLines.contains { $0.hasText && $0.hasCompletedWords(at: milliseconds) } ? index : nil
    }
}
struct LyricsSelection: Codable, Hashable, Sendable {
    let id, title, artist, album: String
    let durationMs: UInt64
    let additionalArtists: [String]?
    init(_ item: MediaItem) {
        id = item.id; title = item.title; artist = item.artists.first ?? item.artist
        album = item.album ?? ""; durationMs = item.durationMs
        let credits = item.artists.dropFirst().prefix(16).filter { !$0.isEmpty && $0.utf8.count <= 512 }
        additionalArtists = credits.isEmpty ? nil : credits
    }
    // Preserve the identity of imports saved before additional credits were available.
    func sameRecording(as other: Self) -> Bool {
        id == other.id && title == other.title && artist == other.artist
            && album == other.album && durationMs == other.durationMs
    }
}
struct LyricsResult: Codable, Sendable, Equatable {
    let document: LyricsDocument
    let source: String
    let sourceURL: URL?
    let imported: Bool
}
enum LyricsFailure: Error, LocalizedError {
    case invalid, missing, network, busy, rateLimited(Int), storage
    var errorDescription: String? {
        switch self {
        case .invalid: "This lyrics file has unsupported or invalid timing. Try LRC, TTML or Lyricsfile 1.0."
        case .missing: "No matching lyrics yet. You can import a lyrics file for this track."
        case .network: "Lyrics could not be loaded. Check your connection and try again."
        case .busy: "The previous lyrics request is finishing. Try again in a moment."
        case .rateLimited(let seconds): "The lyrics service is busy. Try again in \(seconds) seconds."
        case .storage: "Lyrics could not be saved. The original file has not been changed."
        }
    }
}
typealias LyricsProvisionalHandler = @MainActor @Sendable (LyricsResult) -> Void

protocol LyricsProviding: Sendable {
    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult
    func lookup(_ selection: LyricsSelection, provisional: @escaping LyricsProvisionalHandler) async throws -> LyricsResult
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult
}

extension LyricsProviding {
    func lookup(_ selection: LyricsSelection, provisional: @escaping LyricsProvisionalHandler) async throws -> LyricsResult {
        try await lookup(selection)
    }
}
