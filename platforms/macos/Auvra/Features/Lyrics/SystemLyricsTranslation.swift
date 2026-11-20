import NaturalLanguage
import SwiftUI
// The framework session stays inside its SwiftUI task and is used sequentially.
@preconcurrency import Translation

extension View {
    @ViewBuilder func systemLyricsTranslation(_ lyrics: LyricsStore) -> some View {
        if #available(macOS 15, *) {
            modifier(SystemLyricsTranslation(lyrics: lyrics))
        } else {
            let request = lyrics.translationRequest
            task(id: request?.id) {
                guard let request, !Task.isCancelled, lyrics.translationRequest?.id == request.id else { return }
                let sample = LyricsLanguageSample(request)
                guard !sample.matches(request.targetLanguage) else {
                    lyrics.failTranslation(String(localized: "These lyrics are already in your language."), for: request.id)
                    return
                }
                guard let source = sample.source else {
                    lyrics.failTranslation(String(localized: "The lyrics language couldn’t be identified."), for: request.id)
                    return
                }
                guard OfflineLyricsTranslation.isAvailable(source: source, target: request.targetLanguage) else {
                    lyrics.failTranslation(String(localized: "The offline pack doesn’t support these languages."), for: request.id)
                    return
                }
                await translateOffline(request, source: source, lyrics: lyrics)
            }
        }
    }
}

private struct LyricsLanguageSample {
    let text: String
    let source: Locale.Language?

    init(_ request: LyricsTranslationRequest) {
        text = String(request.lines.lazy.map(\.text).joined(separator: "\n").prefix(4096))
        let recognizer = NLLanguageRecognizer()
        recognizer.processString(text)
        source = recognizer.languageHypotheses(withMaximum: 1).first.flatMap { language, confidence in
            confidence >= 0.65 ? Locale.Language(identifier: language.rawValue) : nil
        }
    }

    func matches(_ target: Locale.Language) -> Bool {
        guard let source else { return false }
        return source.languageCode == target.languageCode && source.script == target.script
    }
}

@MainActor
private func translateOffline(_ request: LyricsTranslationRequest, source: Locale.Language, lyrics: LyricsStore) async {
    do {
        let groups = LyricsTranslationGroup.contextual(request.lines)
        let rows = try await OfflineLyricsTranslation.translate(
            groups.map { $0.lines.map(\.text) }, source: source, target: request.targetLanguage
        )
        try Task.checkCancellation()
        guard lyrics.translationRequest?.id == request.id else { return }
        var translated: [Int: String] = [:]
        for (group, result) in zip(groups, rows) {
            for (line, text) in zip(group.lines, result) { translated[line.index] = text }
        }
        lyrics.acceptTranslation(translated, for: request.id)
    } catch is CancellationError {
        lyrics.failTranslation(nil, for: request.id)
    } catch {
        lyrics.failTranslation(String(localized: "The offline translation couldn’t finish. Try again."), for: request.id)
    }
}

@available(macOS 15, *)
private struct SystemLyricsTranslation: ViewModifier {
    private struct Work {
        let request: LyricsTranslationRequest
        var configuration: TranslationSession.Configuration
    }

    let lyrics: LyricsStore
    @State private var work: Work?

    func body(content: Content) -> some View {
        let request = lyrics.translationRequest
        let activeWork = work.flatMap { $0.request.id == request?.id ? $0 : nil }
        content
            .task(id: request?.id) {
                guard !Task.isCancelled, lyrics.translationRequest?.id == request?.id else { return }
                guard let request else { work = nil; return }
                await prepare(request)
            }
            .translationTask(activeWork?.configuration) { session in
                guard let activeWork else { return }
                await translate(activeWork.request, using: session)
            }
    }

    private func prepare(_ request: LyricsTranslationRequest) async {
        let sample = LyricsLanguageSample(request)
        let source = sample.source
        let target = request.targetLanguage
        if sample.matches(target) {
            lyrics.failTranslation(String(localized: "These lyrics are already in your language."), for: request.id)
            return
        }

        var configuration = work?.configuration ?? TranslationSession.Configuration()
        configuration.source = source
        configuration.target = target
        var availability = LanguageAvailability()
        #if compiler(>=6.3)
        if #available(macOS 26.4, *) {
            configuration.preferredStrategy = .highFidelity
            availability = LanguageAvailability(preferredStrategy: .highFidelity)
        }
        #endif
        var status = await languageStatus(availability, source: source, target: target, sample: sample.text)
        #if compiler(>=6.3)
        if #available(macOS 26.4, *), status == .unsupported, isCurrent(request.id) {
            configuration.preferredStrategy = .lowLatency
            status = await languageStatus(LanguageAvailability(preferredStrategy: .lowLatency), source: source, target: target, sample: sample.text)
        }
        #endif
        guard isCurrent(request.id) else {
            lyrics.failTranslation(nil, for: request.id)
            return
        }
        if status != .installed, let source, OfflineLyricsTranslation.isAvailable(source: source, target: target) {
            await translateOffline(request, source: source, lyrics: lyrics)
            return
        }
        guard status != .unsupported else {
            lyrics.failTranslation(String(localized: "Translation is unavailable for these languages."), for: request.id)
            return
        }
        configuration.invalidate()
        work = Work(request: request, configuration: configuration)
    }

    private func languageStatus(_ availability: LanguageAvailability, source: Locale.Language?,
                                target: Locale.Language, sample: String) async -> LanguageAvailability.Status? {
        if let source { return await availability.status(from: source, to: target) }
        // The view-scoped session can ask the user to identify an ambiguous source.
        return try? await availability.status(for: sample, to: target)
    }

    private func translate(_ request: LyricsTranslationRequest, using session: TranslationSession) async {
        do {
            var groups = LyricsTranslationGroup.contextual(request.lines)
            var next = 0
            var translated: [Int: String] = [:]
            while next < groups.count {
                try checkCurrent(request.id)
                let end = min(groups.count, next + 16)
                let batch = Array(groups[next..<end])
                next = end
                let inputs = Self.makeInputs(batch)
                let results = try await session.translations(from: inputs)
                try checkCurrent(request.id)
                guard results.count == batch.count else { throw IncompleteTranslation() }
                for (group, result) in zip(batch, results) {
                    guard result.clientIdentifier == group.identifier else { throw IncompleteTranslation() }
                    if let aligned = group.align(result.targetText) {
                        for (line, text) in zip(group.lines, aligned) {
                            guard translated.updateValue(text, forKey: line.index) == nil else { throw IncompleteTranslation() }
                        }
                    } else {
                        guard group.lines.count > 1 else { throw IncompleteTranslation() }
                        // Preserve row identity when contextual translation reshapes the lines.
                        groups.append(contentsOf: group.lines.map { LyricsTranslationGroup(lines: [$0]) })
                    }
                }
            }
            guard translated.count == request.lines.count else { throw IncompleteTranslation() }
            lyrics.acceptTranslation(translated, for: request.id)
        } catch is CancellationError {
            lyrics.failTranslation(nil, for: request.id)
        } catch let error as CocoaError where error.code == .userCancelled {
            lyrics.failTranslation(nil, for: request.id)
        } catch TranslationError.unsupportedSourceLanguage, TranslationError.unsupportedTargetLanguage, TranslationError.unsupportedLanguagePairing {
            lyrics.failTranslation(String(localized: "Translation is unavailable for these languages."), for: request.id)
        } catch TranslationError.unableToIdentifyLanguage {
            lyrics.failTranslation(String(localized: "The lyrics language couldn’t be identified."), for: request.id)
        } catch {
            let message = Task.isCancelled ? nil : String(localized: "The lyrics couldn’t be translated. Try again.")
            lyrics.failTranslation(message, for: request.id)
        }
    }

    private func checkCurrent(_ id: UInt64) throws {
        guard isCurrent(id) else { throw CancellationError() }
    }

    private func isCurrent(_ id: UInt64) -> Bool {
        !Task.isCancelled && lyrics.translationRequest?.id == id
    }

    private nonisolated static func makeInputs(_ groups: [LyricsTranslationGroup]) -> sending [TranslationSession.Request] {
        groups.map { .init(sourceText: $0.text, clientIdentifier: $0.identifier) }
    }

    private struct IncompleteTranslation: Error {}
}

struct LyricsTranslationGroup: Sendable {
    var lines: [LyricsTranslationLine]
    var identifier: String { String(lines[0].index) }
    var text: String { lines.map(\.text).joined(separator: "\n") }

    static func contextual(_ lines: [LyricsTranslationLine]) -> [Self] {
        var groups: [Self] = []
        for line in lines {
            if let last = groups.last, last.lines.count < 4, last.lines.last?.index == line.index - 1,
               !line.isBacking, !last.lines[0].isBacking,
               !last.lines[0].text.contains(where: \.isNewline), !line.text.contains(where: \.isNewline),
               last.text.utf8.count + line.text.utf8.count + 1 <= 1024 {
                groups[groups.count - 1].lines.append(line)
            } else {
                groups.append(Self(lines: [line]))
            }
        }
        return groups
    }

    func align(_ target: String) -> [String]? {
        guard target.utf8.count <= 16 * 1024 else { return nil }
        let pieces = lines.count == 1 ? [target] : target.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline).map(String.init)
        let aligned = pieces.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        guard aligned.count == lines.count, aligned.allSatisfy({ !$0.isEmpty }) else { return nil }
        return aligned
    }
}
