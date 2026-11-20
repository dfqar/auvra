import AppKit
import Foundation
import Observation
import UniformTypeIdentifiers

/// One lyrics projection shared by main/mini. Selection generations reject late network and import results.
@MainActor @Observable
final class LyricsStore {
    struct Unavailability: Equatable {
        let message: String
        fileprivate let generation: UInt64
    }

    private struct CachedTranslation {
        let targetLanguage: Locale.Language
        let document: LyricsDocument
    }
    private struct Issue {
        let message: String
        let fromLookup: Bool
    }
    private(set) var selection: LyricsSelection?
    private(set) var result: LyricsResult? {
        didSet {
            guard oldValue?.document != result?.document else { return }
            clearTranslationRequest()
            clearNotice()
            cachedTranslation = nil
            showsTranslation = false
            presentationRevision &+= 1
        }
    }
    private(set) var presentationRevision: UInt64 = 0
    private(set) var isLoading = false
    private(set) var isImporting = false
    private var failure: Issue?
    var issue: String? { failure?.message }
    private(set) var isVisible = false
    private(set) var translationRequest: LyricsTranslationRequest?
    private(set) var showsTranslation = false
    private(set) var notice: String?
    private var cachedTranslation: CachedTranslation?
    private var isProvisional = false
    var canTranslate: Bool { !isLoading && !isImporting && result?.document.hasTranslatableText == true }
    var activeTranslationLanguage: Locale.Language? {
        if let request = translationRequest { return request.targetLanguage }
        return showsTranslation ? cachedTranslation?.targetLanguage : nil
    }
    var displayedDocument: LyricsDocument? {
        if showsTranslation, let cachedTranslation { return cachedTranslation.document }
        return result?.document
    }
    var unavailability: Unavailability? {
        guard isVisible, !isLoading, !isImporting else { return nil }
        if let failure {
            return failure.fromLookup ? Unavailability(message: failure.message, generation: generation) : nil
        }
        guard result?.document.instrumental == true else { return nil }
        return Unavailability(message: String(localized: "This track has no lyrics."), generation: generation)
    }
    var offsetSeconds: Double = 0
    var offsetMilliseconds: Double { offsetSeconds * 1000 }
    var followsPlayback = true
    @ObservationIgnored private let provider: any LyricsProviding
    @ObservationIgnored private let noticeDuration: Duration
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var noticeTask: Task<Void, Never>?
    @ObservationIgnored private var generation: UInt64 = 0
    @ObservationIgnored private var translationSerial: UInt64 = 0
    init(provider: any LyricsProviding, noticeDuration: Duration = .seconds(3)) {
        self.provider = provider
        self.noticeDuration = noticeDuration
    }
    func select(_ item: MediaItem?) {
        let next = item.map(LyricsSelection.init)
        guard next != selection else { return }
        cancel(); selection = next; present(nil); failure = nil; offsetSeconds = 0; followsPlayback = true
        if isVisible { load() }
    }
    func setVisible(_ visible: Bool) {
        if visible && !isVisible { failure = nil }
        isVisible = visible
        if visible { if result == nil || isProvisional { load() } }
        else { cancel() }
    }
    func dismissUnavailable(_ unavailable: Unavailability) {
        guard unavailability == unavailable else { return }
        setVisible(false)
    }
    func refreshOnlineLyrics() {
        guard result?.imported != true, !isImporting else { return }
        cancel()
        present(nil)
        failure = nil
        if isVisible { load() }
    }
    func load() {
        guard let selection, !isLoading else { return }
        clearTranslationRequest()
        clearNotice()
        generation &+= 1; let epoch = generation; let provider = provider
        isLoading = true; failure = nil
        task = Task { [weak self] in
            var finishingAttempts = 0
            while !Task.isCancelled {
                guard self?.generation == epoch, self?.selection == selection else { return }
                do {
                    let result = try await provider.lookup(selection, provisional: { [weak self] preview in
                        guard let self, !Task.isCancelled, generation == epoch, self.selection == selection,
                              isLoading, isVisible else { return }
                        if let current = self.result,
                           current.imported || current.document.precision == "word"
                           || (current.document.hasTiming && !preview.document.hasTiming) { return }
                        present(preview, provisional: true)
                    })
                    guard let self, !Task.isCancelled, generation == epoch, self.selection == selection else { return }
                    present(result)
                    isLoading = false; task = nil
                    return
                } catch LyricsFailure.busy where finishingAttempts < 8 {
                    // A cancelled lookup can still be releasing its network slot.
                    // Retry only admission, never HTTP failures or Retry-After.
                    guard !Task.isCancelled, self?.generation == epoch, self?.selection == selection else { return }
                    do { try await Task.sleep(for: .milliseconds(min(400, 40 << finishingAttempts))) }
                    catch { return }
                    finishingAttempts += 1
                } catch {
                    guard let self, !Task.isCancelled, generation == epoch, self.selection == selection else { return }
                    if !isProvisional {
                        reportFailure((error as? LyricsFailure)?.errorDescription ?? "Lyrics could not be loaded. Try again.", fromLookup: true)
                    }
                    isProvisional = false
                    isLoading = false; task = nil
                    return
                }
            }
        }
    }
    func importLyrics() async {
        guard let selection, !isImporting else { return }
        clearTranslationRequest()
        clearNotice()
        isImporting = true; defer { isImporting = false }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = ["lrc", "ttml", "yaml", "yml", "txt"].compactMap { UTType(filenameExtension: $0) }
        panel.message = "Choose lyrics for \(selection.title). LRC, TTML, Lyricsfile YAML or plain UTF-8 text; up to 512 KiB."
        guard await panel.begin() == .OK, let url = panel.url, self.selection == selection else { return }
        await importFile(url)
    }
    func importFile(_ url: URL) async {
        guard let selection else { return }
        cancel(); let epoch = generation; isLoading = true; failure = nil
        do {
            let result = try await provider.importFile(url, for: selection)
            guard !Task.isCancelled, generation == epoch, self.selection == selection else { return }
            present(result); isLoading = false
        } catch {
            guard !Task.isCancelled, generation == epoch else { return }
            reportFailure((error as? LyricsFailure)?.errorDescription ?? "This lyrics file could not be imported.", fromLookup: false)
            isLoading = false
        }
    }
    func cancel() {
        generation &+= 1; task?.cancel(); task = nil; isLoading = false
        clearTranslationRequest()
        clearNotice()
    }
    func toggleTranslation(to language: Locale.Language) {
        guard isVisible, canTranslate, let original = result?.document else { return }
        clearNotice()
        if translationRequest?.targetLanguage == language {
            clearTranslationRequest()
            return
        }
        clearTranslationRequest()
        if cachedTranslation?.targetLanguage == language {
            showsTranslation.toggle()
            return
        }
        cachedTranslation = nil
        showsTranslation = false
        switch original.translationInput() {
        case .unavailable: showNotice("These lyrics could not be translated.")
        case .tooLong: showNotice("These lyrics are too long to translate.")
        case .ready(let lines):
            translationSerial &+= 1
            translationRequest = LyricsTranslationRequest(id: translationSerial, targetLanguage: language, lines: lines)
        }
    }
    func acceptTranslation(_ translations: [Int: String], for id: UInt64) {
        guard let request = translationRequest, request.id == id, let original = result?.document else { return }
        clearTranslationRequest()
        guard let translated = original.applyingTranslation(translations, to: request.lines) else {
            showsTranslation = false
            showNotice("The translation was incomplete or too long.")
            return
        }
        cachedTranslation = CachedTranslation(targetLanguage: request.targetLanguage, document: translated)
        showsTranslation = true
    }
    func failTranslation(_ message: String?, for id: UInt64) {
        guard translationRequest?.id == id else { return }
        clearTranslationRequest()
        if let message { showNotice(message) }
    }
    private func clearTranslationRequest() {
        translationRequest = nil
    }
    private func present(_ result: LyricsResult?, provisional: Bool = false) {
        isProvisional = provisional
        if self.result != result { self.result = result }
    }
    private func reportFailure(_ message: String, fromLookup: Bool) {
        if result == nil || result?.document.instrumental == true {
            failure = Issue(message: message, fromLookup: fromLookup)
        }
        else { showNotice(message) }
    }
    private func showNotice(_ message: String) {
        guard isVisible, !message.isEmpty else { return }
        noticeTask?.cancel()
        notice = String(message.prefix(512))
        noticeTask = Task { [weak self, noticeDuration] in
            do { try await Task.sleep(for: noticeDuration) }
            catch { return }
            guard !Task.isCancelled else { return }
            self?.clearNotice()
        }
    }
    private func clearNotice() {
        noticeTask?.cancel()
        noticeTask = nil
        notice = nil
    }
    func adjustOffset(_ delta: Double) { guard delta.isFinite else { return }; offsetSeconds = min(10, max(-10, offsetSeconds + delta)) }
    func playbackPosition(forLineStart start: UInt64) -> UInt64 {
        let target = max(0, (Double(start) - offsetMilliseconds).rounded())
        return target >= Double(UInt64.max) ? UInt64.max : UInt64(target)
    }
}
