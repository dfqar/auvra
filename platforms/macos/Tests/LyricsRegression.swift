import AppKit
import Foundation

private final class LyricsHTTPReplies: @unchecked Sendable {
    static let shared = LyricsHTTPReplies()
    private let lock = NSLock()
    private var status = 200, body = Data()
    private var headers: [String: String] = [:]
    private var requests: [URLRequest] = []
    private var byHost: [String: (Int, Data, [String: String], TimeInterval)] = [:]
    private var byPath: [String: (Int, Data, [String: String], TimeInterval)] = [:]
    func reset(_ code: Int = 200, _ data: Data, headers: [String: String] = [:]) {
        lock.lock(); defer { lock.unlock() }; status = code; body = data; self.headers = headers; requests = []; byHost = [:]; byPath = [:]
    }
    func set(host: String, status: Int = 200, body: Data, headers: [String: String] = [:], delay: TimeInterval = 0) {
        lock.lock(); defer { lock.unlock() }; byHost[host] = (status, body, headers, delay)
    }
    func set(path: String, status: Int = 200, body: Data, headers: [String: String] = [:], delay: TimeInterval = 0) {
        lock.lock(); defer { lock.unlock() }; byPath[path] = (status, body, headers, delay)
    }
    func take(_ request: URLRequest) -> (Int, Data, [String: String], TimeInterval) {
        lock.lock(); defer { lock.unlock() }; requests.append(request)
        if let path = request.url?.path, let reply = byPath[path] { return reply }
        if request.url?.host == "lrclib.net", request.url?.path == "/api/search" { return (200, Data("[]".utf8), [:], 0) }
        return byHost[request.url?.host ?? ""] ?? (status, body, headers, 0)
    }
    func recorded() -> [URLRequest] { lock.lock(); defer { lock.unlock() }; return requests }
}
private final class LyricsHTTPProtocol: URLProtocol, @unchecked Sendable {
    private var work: DispatchWorkItem?
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        let (status, body, headers, delay) = LyricsHTTPReplies.shared.take(request)
        let pending = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.client?.urlProtocol(self, didReceive: HTTPURLResponse(url: self.request.url!, statusCode: status, httpVersion: "HTTP/1.1", headerFields: headers.merging(["Content-Type": "application/json"]) { a, _ in a })!, cacheStoragePolicy: .notAllowed)
            self.client?.urlProtocol(self, didLoad: body); self.client?.urlProtocolDidFinishLoading(self)
        }
        work = pending
        DispatchQueue.global().asyncAfter(deadline: .now() + delay, execute: pending)
    }
    override func stopLoading() { work?.cancel() }
}
actor OriginalLyricsFixture: LyricsProviding {
    static let overlappingVoices = """
        <tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata">
          <body dur="12s"><div begin="1s" end="12s">
            <p begin="2s" end="8s">
              <span begin="2s" end="4s">Hold the </span><span begin="5s" end="8s">light</span>
              <span ttm:role="x-bg"><span begin="4.5s" end="6s">Soft </span><span begin="6s" end="9s">echo</span></span>
            </p>
            <p begin="9.4s" end="12s">Then let the daylight in</p>
          </div></body>
        </tt>
        """
    static let typographyLines: [LyricsLine] = [
        ["אור ", "שקט ", "בבית"],
        ["نور ", "هادئ ", "في ", "المساء"],
        ["Soft ", "אור ", "with ", "Cafe\u{301} ", "👨‍👩‍👧‍👦"],
        ["Сверхдлинноеоригинальноесловодляпроверкипереносов"]
    ].map { tokens in
        let duration: UInt64 = tokens.count == 1 ? 8000 : 2000
        return LyricsLine(text: tokens.joined(), startMs: 0, endMs: UInt64(tokens.count) * duration, words: tokens.enumerated().map { index, text in
            LyricsWord(text: text, startMs: UInt64(index) * duration, endMs: UInt64(index + 1) * duration, endEstimated: false)
        })
    }
    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult {
        let text = """
        version: '1.0'
        lines:
          - text: 'Пусть этот свет останется'
            start_ms: 1001
            end_ms: 9000
            words:
              - {text: 'Пусть ', start_ms: 1001, end_ms: 1800}
              - {text: 'этот ', start_ms: 1800, end_ms: 2400}
              - {text: 'свет ', start_ms: 2400, end_ms: 6500}
              - {text: 'останется', start_ms: 6500, end_ms: 9000}
          - {text: 'И тихо поведёт домой', start_ms: 9500, end_ms: 14000}
          - {text: 'Сквозь город и его огни', start_ms: 15000, end_ms: 19500}
          - {text: 'Мы слышим музыку внутри', start_ms: 21000, end_ms: 28000}
        """
        return LyricsResult(document: try LyricsService.parse(text, format: "lyricsfile", duration: max(30000, selection.durationMs)), source: "Original Auvra fixture", sourceURL: nil, imported: false)
    }
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult { try await lookup(selection) }
}
private actor DelayedLyrics: LyricsProviding {
    var pending: [String: CheckedContinuation<LyricsResult, any Error>] = [:]
    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult { try await withCheckedThrowingContinuation { pending[selection.id] = $0 } }
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult { try await lookup(selection) }
    func ready(_ id: String) -> Bool { pending[id] != nil }
    func resolve(_ id: String, result: LyricsResult) { pending.removeValue(forKey: id)?.resume(returning: result) }
}
private actor ProvisionalLyrics: LyricsProviding {
    private typealias Preview = @MainActor @Sendable (LyricsResult) -> Void
    private var previews: [Int: Preview] = [:]
    private var pending: [Int: CheckedContinuation<LyricsResult, any Error>] = [:]
    private(set) var attempts = 0

    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult { throw LyricsFailure.missing }
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult { throw LyricsFailure.missing }
    func lookup(_ selection: LyricsSelection, provisional: @escaping @MainActor @Sendable (LyricsResult) -> Void) async throws -> LyricsResult {
        attempts += 1
        let attempt = attempts
        previews[attempt] = provisional
        return try await withCheckedThrowingContinuation { pending[attempt] = $0 }
    }
    func publish(_ result: LyricsResult, on attempt: Int) async { await previews[attempt]?(result) }
    func finish(_ result: Result<LyricsResult, LyricsFailure>, on attempt: Int) {
        previews.removeValue(forKey: attempt)
        let continuation = pending.removeValue(forKey: attempt)
        continuation?.resume(with: result.mapError { $0 as any Error })
    }
}
private actor FinishingLyrics: LyricsProviding {
    private let heldID: String, result: LyricsResult
    private var pending: CheckedContinuation<LyricsResult, any Error>?
    private var released = false
    private(set) var attempts: [String: Int] = [:]
    init(heldID: String, result: LyricsResult) { self.heldID = heldID; self.result = result }
    func ready() -> Bool { pending != nil }
    func finish() { released = true; pending?.resume(returning: result); pending = nil }
    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult {
        attempts[selection.id, default: 0] += 1
        if selection.id == heldID && !released { return try await withCheckedThrowingContinuation { pending = $0 } }
        guard released else { throw LyricsFailure.busy }
        return result
    }
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult { try await lookup(selection) }
}
actor FailingLyrics: LyricsProviding {
    let failure: LyricsFailure
    let delay: Duration?
    private(set) var attempts = 0
    init(_ failure: LyricsFailure, delay: Duration? = nil) { self.failure = failure; self.delay = delay }
    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult {
        attempts += 1
        if let delay { try await Task.sleep(for: delay) }
        throw failure
    }
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult { try await lookup(selection) }
}
@MainActor enum LyricsChecks {
    static func run(item: MediaItem, directory: URL) async throws -> [String] {
        var passed: [String] = []
        func check(_ value: Bool, _ message: String) throws {
            guard value else { throw NSError(domain: "LyricsRegression", code: 1, userInfo: [NSLocalizedDescriptionKey: message]) }
            passed.append(message)
        }
        func wait(attempts: Int = 100, _ condition: () async -> Bool) async throws {
            for _ in 0..<attempts { if await condition() { return }; try await Task.sleep(for: .milliseconds(10)) }
            throw NSError(domain: "LyricsRegression timeout", code: 1)
        }
        let selection = LyricsSelection(item)
        let fixture = try await OriginalLyricsFixture().lookup(selection)
        let word = fixture.document.lines[0].words[2]
        try check(word.progress(at: 2400) == 0 && abs(word.progress(at: 4450) - 0.5) < 0.001 && word.progress(at: 6500) == 1, "Sustained word sweep follows its real 4.1-second interval")
        try check(word.progress(at: -100) == 0 && word.progress(at: .infinity) == 0, "Lyrics progress handles preroll and invalid clocks")
        try check(fixture.document.focusedLine(at: 0) == nil && fixture.document.focusedLine(at: 9000) == 0
                  && fixture.document.focusedLine(at: 9499) == 0 && fixture.document.focusedLine(at: 9500) == 1
                  && fixture.document.focusedLine(at: 14_500) == nil,
                  "Completed word lyrics keep focus up to the next cue, and a later line-timed gap does not revive them")
        let lastWordDocument = LyricsDocument(lines: [fixture.document.lines[0]], instrumental: false, precision: "word")
        try check(lastWordDocument.focusedLine(at: 9100, paused: true) == 0
                  && lastWordDocument.focusedLine(at: 15000) == 0
                  && lastWordDocument.focusedLine(at: 900, paused: true) == nil,
                  "The final word-timed row holds through the outro and a backward seek clears it")
        try check(fixture.document.focusedLine(at: 1000, paused: true) == 0 && fixture.document.focusedLine(at: 999, paused: true) == nil
                  && fixture.document.focusedLine(at: 1000) == nil && fixture.document.focusedLine(at: 8999, paused: true) == 0
                  && fixture.document.lines.first?.words.first?.progress(at: 1000) == 0,
                  "Paused lyrics focus tolerates one millisecond of output rounding without advancing the word fill")
        let translated = LyricsStore(provider: OriginalLyricsFixture())
        let english = Locale.Language(identifier: "en")
        translated.select(item); translated.setVisible(true)
        try await wait { translated.result != nil }
        translated.adjustOffset(0.25)
        translated.toggleTranslation(to: english)
        guard let request = translated.translationRequest else {
            throw NSError(domain: "LyricsRegression missing translation request", code: 1)
        }
        try check(request.lines.map(\.index) == Array(0..<4) && request.targetLanguage.languageCode?.identifier == "en"
                  && translated.displayedDocument == fixture.document,
                  "Translation captures the chosen language and original lines while showing the original during work")
        translated.acceptTranslation([0: "May this light stay with us"], for: request.id)
        try check(!translated.showsTranslation && translated.displayedDocument == fixture.document && translated.notice != nil,
                  "An incomplete translation never replaces synchronized lyrics")
        translated.toggleTranslation(to: english)
        try check(translated.notice == nil, "A new translation clears the previous transient notice")
        guard let retry = translated.translationRequest else {
            throw NSError(domain: "LyricsRegression missing translation retry", code: 1)
        }
        let englishLines = [0: "May this light stay with us", 1: "And gently lead us home", 2: "Through the city and its lights", 3: "We can hear the music within"]
        translated.acceptTranslation(englishLines, for: retry.id)
        let translatedDocument = translated.displayedDocument
        try check(translated.showsTranslation && translatedDocument?.precision == "line"
                  && translatedDocument?.lines[0].text == englishLines[0]
                  && translatedDocument?.lines.map(\.startMs) == fixture.document.lines.map(\.startMs)
                  && translatedDocument?.lines.map(\.endMs) == fixture.document.lines.map(\.endMs)
                  && translatedDocument?.lines.allSatisfy { $0.words.isEmpty } == true
                  && translated.offsetMilliseconds == 250 && translated.result?.document == fixture.document,
                  "Translation keeps the original line clock and offset while leaving the original word timing untouched")
        translated.toggleTranslation(to: english)
        try check(!translated.showsTranslation && translated.displayedDocument == fixture.document,
                  "Show original restores the exact word-timed document")
        translated.toggleTranslation(to: english)
        try check(translated.showsTranslation && translated.translationRequest == nil && translated.displayedDocument == translatedDocument,
                  "Toggling back to a completed translation reuses the in-memory result")
        translated.toggleTranslation(to: Locale.Language(identifier: "fr"))
        guard let pendingTranslation = translated.translationRequest else {
            throw NSError(domain: "LyricsRegression missing second translation request", code: 1)
        }
        translated.setVisible(false)
        translated.acceptTranslation(englishLines, for: pendingTranslation.id)
        try check(translated.translationRequest == nil && !translated.showsTranslation && translated.displayedDocument == fixture.document,
                  "Closing Lyrics cancels the pending language change and rejects its late result")
        translated.setVisible(true)
        translated.toggleTranslation(to: english)
        guard let oldTrackTranslation = translated.translationRequest else {
            throw NSError(domain: "LyricsRegression missing track-scoped translation", code: 1)
        }
        translated.select(nil)
        translated.acceptTranslation(englishLines, for: oldTrackTranslation.id)
        try check(translated.translationRequest == nil && translated.displayedDocument == nil,
                  "A previous track’s completed translation cannot reach the new selection")
        let noticeStore = LyricsStore(provider: OriginalLyricsFixture(), noticeDuration: .milliseconds(80))
        let russian = Locale.Language(identifier: "ru")
        let sameLanguage = "These lyrics are already in your language."
        noticeStore.select(item); noticeStore.setVisible(true)
        try await wait { noticeStore.result != nil }
        noticeStore.toggleTranslation(to: russian)
        guard let noticeRequest = noticeStore.translationRequest else {
            throw NSError(domain: "LyricsRegression missing notice request", code: 1)
        }
        noticeStore.failTranslation(sameLanguage, for: noticeRequest.id)
        try check(noticeStore.notice == sameLanguage && noticeStore.displayedDocument == fixture.document && noticeStore.issue == nil,
                  "A same-language notice preserves the available lyrics")
        try await wait { noticeStore.notice == nil }
        try check(noticeStore.notice == nil && noticeStore.displayedDocument == fixture.document,
                  "A same-language notice dismisses itself without changing lyrics")
        noticeStore.toggleTranslation(to: russian)
        guard let closingNoticeRequest = noticeStore.translationRequest else {
            throw NSError(domain: "LyricsRegression missing closing notice request", code: 1)
        }
        noticeStore.failTranslation(sameLanguage, for: closingNoticeRequest.id)
        noticeStore.setVisible(false)
        noticeStore.failTranslation(sameLanguage, for: closingNoticeRequest.id)
        try check(noticeStore.notice == nil, "Closing lyrics clears the notice and rejects late failures")
        let groups = LyricsTranslationGroup.contextual([0, 1, 2, 3, 4, 6].map {
            LyricsTranslationLine(index: $0, text: "Original line \($0)")
        })
        try check(groups.map { $0.lines.map(\.index) } == [[0, 1, 2, 3], [4], [6]],
                  "Contextual translation groups adjacent lines without bridging a stanza break")
        try check(groups[0].align("One\r\nTwo\r\nThree\r\nFour") == ["One", "Two", "Three", "Four"]
                  && groups[0].align("Merged lyric lines") == nil,
                  "Translation keeps exact line alignment and falls back if the engine reshapes a stanza")
        let config = URLSessionConfiguration.ephemeral; config.protocolClasses = [LyricsHTTPProtocol.self]
        let session = URLSession(configuration: config)
        let service = LyricsService(directory: directory, session: session, userAgent: "AuvraTest/1 (https://example.invalid)", enableUnison: false)
        func reply(id: Int = 123, title: String = item.title, artist: String = item.artists[0],
                   album: String = item.album ?? "", duration: Double = Double(item.durationMs) / 1000,
                   instrumental: Bool = false, synced: String? = "[00:01]Our original line\n[00:05]Home again",
                   plain: String? = "Our original line\nHome again", yaml: String? = nil) -> [String: Any] {
            var object: [String: Any] = ["id": id, "trackName": title, "artistName": artist, "albumName": album,
                                         "duration": duration, "instrumental": instrumental]
            object["syncedLyrics"] = synced
            object["plainLyrics"] = plain
            object["lyricsfile"] = yaml
            return object
        }
        func response(title: String = item.title, artist: String = item.artists[0], duration: Double = Double(item.durationMs) / 1000,
                      instrumental: Bool = false, yaml: String? = nil) throws -> Data {
            try JSONSerialization.data(withJSONObject: reply(title: title, artist: artist, duration: duration, instrumental: instrumental, yaml: yaml))
        }
        func search(_ candidates: [[String: Any]], delay: TimeInterval = 0) throws {
            LyricsHTTPReplies.shared.set(path: "/api/search", body: try JSONSerialization.data(withJSONObject: candidates), delay: delay)
        }
        LyricsHTTPReplies.shared.reset(200, try response())
        let online = try await service.lookup(selection)
        let requests = LyricsHTTPReplies.shared.recorded()
        try check(online.document.precision == "line" && online.source == "LRCLIB", "Online LRC fallback preserves line precision")
        let lrclibRequests = requests.filter { $0.url?.host == "lrclib.net" }
        try check(lrclibRequests.count == 1 && lrclibRequests[0].value(forHTTPHeaderField: "Authorization") == nil && lrclibRequests[0].value(forHTTPHeaderField: "User-Agent")?.contains("AuvraTest") == true, "Lyrics HTTP sends only metadata and client identity, without Spotify credentials")
        _ = try await service.lookup(selection)
        try check(LyricsHTTPReplies.shared.recorded().count == requests.count, "Repeated lyrics opening uses bounded memory cache")
        let fresh = { LyricsService(directory: directory.appendingPathComponent(UUID().uuidString), session: session, enableUnison: false) }
        LyricsHTTPReplies.shared.reset(200, try response(title: "Wrong recording"))
        do { _ = try await fresh().lookup(selection); try check(false, "Mismatched title rejected") } catch LyricsFailure.missing { try check(true, "Mismatched recording title is not shown") }
        LyricsHTTPReplies.shared.reset(200, try response(duration: Double(item.durationMs)/1000 + 5))
        do { _ = try await fresh().lookup(selection); try check(false, "Wrong version rejected") } catch LyricsFailure.missing { try check(true, "Duration mismatch does not attach lyrics to a different version") }
        LyricsHTTPReplies.shared.reset(200, try response(instrumental: true))
        try check(try await fresh().lookup(selection).document.instrumental, "Instrumental response is distinct from missing lyrics")

        let studioAlbum = "Original studio album"
        let collaboration = MediaItem(id: "original-lyrics-collaboration", title: "Day + night", artists: ["Original lead", "Original guest"],
                                      album: studioAlbum, durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities)
        let collaborationSelection = LyricsSelection(collaboration)
        let matchingArtist = collaboration.artists.joined(separator: ", ")
        var unknownDuration = reply(id: 199, title: collaboration.title, artist: matchingArtist, album: studioAlbum)
        unknownDuration["duration"] = NSNull()
        LyricsHTTPReplies.shared.reset(404, Data())
        try search([
            ["id": 198, "trackName": NSNull()],
            unknownDuration,
            reply(id: 201, title: collaboration.title + " - Live", artist: matchingArtist, album: studioAlbum),
            reply(id: 202, title: collaboration.title, artist: "Different lead", album: studioAlbum),
            reply(id: 203, title: collaboration.title, artist: matchingArtist, album: studioAlbum, duration: Double(item.durationMs) / 1000 + 5),
            reply(id: 204, title: collaboration.title, artist: matchingArtist, album: "Original compilation"),
            reply(id: 205, title: collaboration.title, artist: matchingArtist, album: studioAlbum, synced: "[00:02.15]Our original line\n[00:05.25]Home again")
        ])
        let searchService = fresh(), recovered = try await searchService.lookup(collaborationSelection)
        try check(recovered.document.precision == "line" && recovered.document.lines.map(\.startMs) == [2150, 5250]
                  && recovered.sourceURL?.lastPathComponent == "205",
                  "Search recovers real line cues after an exact miss, skipping incomplete rows and preferring the matching album")
        let fallbackRequests = LyricsHTTPReplies.shared.recorded()
        let searchURL = fallbackRequests.last?.url.flatMap { URLComponents(url: $0, resolvingAgainstBaseURL: false) }
        let searchParameters = searchURL?.queryItems
        try check(fallbackRequests.compactMap { $0.url?.path } == ["/api/get", "/api/search"]
                  && searchParameters?.first(where: { $0.name == "track_name" })?.value == collaboration.title
                  && searchParameters?.first(where: { $0.name == "artist_name" })?.value == collaboration.artists[0]
                  && searchParameters?.contains(where: { $0.name == "album_name" || $0.name == "duration" }) == false
                  && searchURL?.percentEncodedQuery?.contains("%2B") == true
                  && fallbackRequests.last?.value(forHTTPHeaderField: "Authorization") == nil
                  && fallbackRequests.last?.value(forHTTPHeaderField: "Cookie") == nil,
                  "The bounded search uses track and primary artist metadata without credentials")
        _ = try await searchService.lookup(collaborationSelection)
        try check(LyricsHTTPReplies.shared.recorded().count == fallbackRequests.count, "Recovered line lyrics use the existing memory cache")

        let localItem = MediaItem(id: "original-local-lyrics", title: item.title, artists: item.artists, album: item.album,
                                  durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities)
        let localSelection = LyricsSelection(localItem)
        let plainReply = try JSONSerialization.data(withJSONObject: reply(synced: "[00:malformed]Our original line"))
        LyricsHTTPReplies.shared.reset(200, plainReply)
        try search([reply(id: 206, synced: "[00:01.35]Our original line\n[00:05.10]Home again")])
        let upgraded = try await fresh().lookup(localSelection)
        try check(upgraded.document.precision == "line" && upgraded.document.lines.first?.startMs == 1350
                  && upgraded.sourceURL?.lastPathComponent == "206",
                  "A verified search result upgrades plain lyrics when the exact response contains malformed timing")

        LyricsHTTPReplies.shared.reset(200, plainReply)
        try search([
            reply(id: 207, title: item.title + " - Live"),
            reply(id: 208, artist: "Different artist"),
            reply(id: 209, duration: Double(item.durationMs) / 1000 + 5),
            reply(id: 210, instrumental: true)
        ])
        let plain = try await fresh().lookup(localSelection)
        try check(plain.document.precision == "plain" && !plain.document.instrumental && !plain.document.hasTiming
                  && plain.document.lines.map(\.text) == ["Our original line", "Home again"]
                  && plain.document.lines.allSatisfy { $0.seekStartMs == nil }
                  && plain.document.focusedLine(at: 1350) == nil,
                  "Wrong versions and instrumental search hits leave exact text readable without synthetic cues")

        LyricsHTTPReplies.shared.reset(404, Data())
        try search([reply(id: 211, instrumental: true)])
        do { _ = try await fresh().lookup(localSelection); try check(false, "Search-only instrumental should be ignored") }
        catch LyricsFailure.missing { try check(true, "An unverified instrumental search hit does not classify the track") }

        LyricsHTTPReplies.shared.reset(200, plainReply)
        LyricsHTTPReplies.shared.set(path: "/api/search", status: 503, body: Data())
        try check(try await fresh().lookup(localSelection).document == plain.document, "A search HTTP failure preserves the available untimed text")

        let plainYAML = "version: '1.0'\nplain: 'Original plain upload'\n"
        LyricsHTTPReplies.shared.reset(200, try response(yaml: plainYAML))
        try check(try await fresh().lookup(localSelection).document.precision == "line"
                  && LyricsHTTPReplies.shared.recorded().compactMap { $0.url?.path } == ["/api/get"],
                  "A plain Lyricsfile never hides synced LRC supplied by the same exact reply")

        LyricsHTTPReplies.shared.reset(200, plainReply)
        try search([reply(id: 212)], delay: 4)
        let cancellingSearchService = fresh(), cancellingSearch = Task { try await cancellingSearchService.lookup(localSelection) }
        try await wait { LyricsHTTPReplies.shared.recorded().contains { $0.url?.path == "/api/search" } }
        cancellingSearch.cancel()
        do { _ = try await cancellingSearch.value; try check(false, "Search cancellation expected") } catch is CancellationError { }
        LyricsHTTPReplies.shared.reset(200, try response())
        try check(try await cancellingSearchService.lookup(localSelection).sourceURL?.lastPathComponent == "123"
                  && LyricsHTTPReplies.shared.recorded().compactMap { $0.url?.path } == ["/api/get"],
                  "Cancelling search releases the request slot without caching stale lyrics")

        let supplemental = { LyricsService(directory: directory.appendingPathComponent(UUID().uuidString), session: session) }
        func unisonFixture(duration: Double = Double(item.durationMs) / 1000, detailVideoID: String = "AAAAAAAAAAA",
                           detailDelay: TimeInterval = 0) throws {
            let identity: [String: Any] = ["id": 313, "videoId": "AAAAAAAAAAA", "song": item.title,
                                           "artist": item.artists[0], "album": item.album ?? "",
                                           "format": "lrc", "syncType": "richsync"]
            var summary = identity
            summary["duration"] = duration
            let summaries = try JSONSerialization.data(withJSONObject: ["success": true, "data": [summary]])
            var detail = identity
            detail["videoId"] = detailVideoID
            detail["lyrics"] = "[00:01]<00:01>Our <00:02>light<00:04>"
            let lyrics = try JSONSerialization.data(withJSONObject: ["success": true, "data": detail])
            LyricsHTTPReplies.shared.set(path: "/lyrics/search", body: summaries)
            LyricsHTTPReplies.shared.set(path: "/lyrics/313", body: lyrics, delay: detailDelay)
        }

        LyricsHTTPReplies.shared.reset(200, plainReply)
        try search([reply(id: 213)])
        try unisonFixture(detailDelay: 0.4)
        let extra = try await supplemental().lookup(localSelection)
        try check(extra.source == "Unison" && extra.document.precision == "word" && extra.document.lines.first?.words.last?.endMs == 4000
                  && LyricsHTTPReplies.shared.recorded().contains { $0.url?.path == "/api/search" }
                  && LyricsHTTPReplies.shared.recorded().allSatisfy { $0.value(forHTTPHeaderField: "Authorization") == nil && $0.value(forHTTPHeaderField: "Cookie") == nil },
                  "Unison word timing upgrades exact plain and search line lyrics without credentials")

        LyricsHTTPReplies.shared.reset(200, plainReply)
        try unisonFixture(duration: Double(item.durationMs) / 1000 + 6)
        try check(try await supplemental().lookup(localSelection).document == plain.document
                  && !LyricsHTTPReplies.shared.recorded().contains { $0.url?.path == "/lyrics/313" },
                  "Unison rejects mismatched recording duration before downloading lyrics")

        LyricsHTTPReplies.shared.reset(200, plainReply)
        try unisonFixture(detailVideoID: "BBBBBBBBBBB")
        try check(try await supplemental().lookup(localSelection).document == plain.document,
                  "Unison rejects lyric details with a different video identity")

        LyricsHTTPReplies.shared.reset(520, Data())
        try unisonFixture()
        try check(try await supplemental().lookup(localSelection).source == "Unison"
                  && !LyricsHTTPReplies.shared.recorded().contains { $0.url?.path == "/api/search" },
                  "Unison can recover after a primary HTTP 520 without retrying the blocked service")

        LyricsHTTPReplies.shared.reset(200, try response())
        try unisonFixture()
        try check(try await supplemental().lookup(localSelection).source == "LRCLIB"
                  && LyricsHTTPReplies.shared.recorded().compactMap { $0.url?.path } == ["/api/get"],
                  "Exact synced lyrics return immediately without sharing metadata with another source")

        let providerContact = LrcRedContact(projectURL: "https://example.invalid/auvra", email: "lyrics@example.invalid")
        try check(providerContact != nil && LrcRedContact(projectURL: "https://example.invalid/auvra", email: "line\nbreak@example.invalid") == nil,
                  "The optional lrc.red contact accepts a public HTTPS address and rejects malformed header input")
        let redService = {
            LyricsService(directory: directory.appendingPathComponent(UUID().uuidString), session: session, enableUnison: false,
                          lrcRedContact: providerContact)
        }
        let isrc = "GBZZZ2600001"
        func lrcRedFixture(duration: Double = Double(item.durationMs) / 1000) throws {
            let hit: [String: Any] = ["isrc": isrc, "title": item.title, "artist": item.artists[0], "album": item.album ?? "", "duration": duration]
            let data = try JSONSerialization.data(withJSONObject: ["hits": [hit]])
            LyricsHTTPReplies.shared.set(path: "/search.json", body: data)
            LyricsHTTPReplies.shared.set(path: "/s/\(isrc).ttml", status: 404, body: Data())
            LyricsHTTPReplies.shared.set(path: "/s/\(isrc).lrc", body: Data("[00:01]Our original line\n[00:05]Home again".utf8),
                                         headers: ["Content-Type": "text/plain"])
        }
        LyricsHTTPReplies.shared.reset(404, Data())
        try lrcRedFixture()
        let lrcRedResult = try await redService().lookup(localSelection)
        let redRequests = LyricsHTTPReplies.shared.recorded()
        try check(lrcRedResult.source == "lrc.red" && lrcRedResult.document.precision == "line"
                  && lrcRedResult.document.lines.map(\.startMs) == [1000, 5000]
                  && lrcRedResult.sourceURL?.path == "/s/\(isrc)"
                  && redRequests.filter { $0.url?.host == "lrc.red" }.allSatisfy { $0.value(forHTTPHeaderField: "User-Agent")?.contains("lyrics@example.invalid") == true }
                  && redRequests.filter { $0.url?.host == "lrclib.net" }.allSatisfy { $0.value(forHTTPHeaderField: "User-Agent")?.contains("lyrics@example.invalid") == false },
                  "Configured lrc.red recovers timed LRC and receives the project contact only on its own host")

        LyricsHTTPReplies.shared.reset(200, plainReply)
        try lrcRedFixture(duration: Double(item.durationMs) / 1000 + 6)
        try check(try await redService().lookup(localSelection).document == plain.document
                  && !LyricsHTTPReplies.shared.recorded().contains { $0.url?.path.hasPrefix("/s/") == true },
                  "lrc.red skips a different recording before downloading its timings")

        try await checkMusixmatch(item: localItem, directory: directory, session: session,
                                 primaryReply: plainReply, primaryDocument: plain.document, check: check)

        let indexedItem = MediaItem(id: "spotify:track:AAAAAAAAAAAAAAAAAAAAAA", title: item.title, artists: item.artists, album: item.album,
                                    durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities, location: nil)
        let indexedSelection = LyricsSelection(indexedItem)
        func xml(_ text: String) -> String { text.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "\"", with: "&quot;").replacingOccurrences(of: "<", with: "&lt;") }
        func indexedBody(id: String = "AAAAAAAAAAAAAAAAAAAAAA", title: String = item.title, artist: String = item.artists[0], album: String = item.album ?? "", duration: UInt64 = item.durationMs) -> Data {
            Data("""
            <tt xmlns="http://www.w3.org/ns/ttml" xmlns:amll="http://www.example.com/ns/amll" xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><head><metadata><amll:meta key="spotifyId" value="\(xml(id))"/><amll:meta key="musicName" value="\(xml(title))"/><amll:meta key="artists" value="\(xml(artist))"/><amll:meta key="album" value="\(xml(album))"/></metadata></head><body dur="\(Double(duration) / 1000)"><div begin="2" end="8.1"><p begin="2" end="8.1"><span begin="2" end="4">Our </span><span begin="4" end="8.1">light</span><span ttm:role="x-translation">Original translation fixture</span></p></div></body></tt>
            """.utf8)
        }
        func indexedReply(_ body: Data, status: Int = 200, mime: String = "text/plain", delay: TimeInterval = 0, headers: [String: String] = [:]) {
            LyricsHTTPReplies.shared.set(host: "raw.githubusercontent.com", status: status, body: body,
                                         headers: headers.merging(["Content-Type": mime]) { a, _ in a }, delay: delay)
        }
        LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(indexedBody(), delay: 0.03)
        let indexedService = fresh(), indexedResult = try await indexedService.lookup(indexedSelection)
        try check(indexedResult.source == "AMLL TTML DB" && indexedResult.document.precision == "word"
                  && indexedResult.document.lines[0].text == "Our light" && indexedResult.document.lines[0].words[1].startMs == 4000
                  && indexedResult.document.lines[0].words[1].endMs == 8100,
                  "Indexed word lyrics preserve song-absolute 4.1-second timing without singing translations")
        let indexedRequests = LyricsHTTPReplies.shared.recorded()
        try check(indexedRequests.count == 2 && indexedRequests.contains { $0.url?.host == "raw.githubusercontent.com" && $0.url?.path.hasSuffix("/AAAAAAAAAAAAAAAAAAAAAA.ttml") == true }
                  && indexedRequests.allSatisfy { $0.value(forHTTPHeaderField: "Authorization") == nil && $0.value(forHTTPHeaderField: "Cookie") == nil }
                  && indexedResult.sourceURL?.host == "github.com",
                  "Indexed lyrics use one exact public track ID, no credentials and a visible source link")
        _ = try await indexedService.lookup(indexedSelection)
        try check(LyricsHTTPReplies.shared.recorded().count == 2, "Reopening indexed lyrics uses the existing bounded cache")
        let localizedCredit = "Original localized artist"
        let alternateCreditItem = MediaItem(id: indexedItem.id, title: item.title, artists: [localizedCredit, item.artists[0]], album: item.album,
                                            durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities)
        LyricsHTTPReplies.shared.reset(200, try response(artist: localizedCredit))
        indexedReply(indexedBody(duration: 8100), delay: 0.03)
        let alternateResult = try await fresh().lookup(LyricsSelection(alternateCreditItem))
        try check(alternateResult.source == "AMLL TTML DB" && alternateResult.document.lines[0].words[1].endMs == 8100,
                  "A shared credited artist and an instrumental tail retain exact-ID word timing")
        let lrclibURL = LyricsHTTPReplies.shared.recorded().first { $0.url?.host == "lrclib.net" }?.url
        let artistParameters = lrclibURL.flatMap { URLComponents(url: $0, resolvingAgainstBaseURL: false) }?.queryItems?
            .filter { $0.name.contains("artist") }.compactMap(\.value)
        try check(artistParameters == [localizedCredit], "LRCLIB still receives only the first artist credit")
        for body in [indexedBody(id: "BBBBBBBBBBBBBBBBBBBBBB"), indexedBody(title: "Wrong recording"), indexedBody(artist: "Another artist"),
                     indexedBody(album: "Wrong album"), indexedBody(duration: item.durationMs + 3000),
                     Data("<!DOCTYPE tt [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><tt>&x;</tt>".utf8)] {
            LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(body)
            try check(try await fresh().lookup(indexedSelection).source == "LRCLIB", "Unmatched or invalid indexed lyrics fall back to verified LRCLIB metadata")
        }
        for status in [404, 403, 429, 503] {
            LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(Data(), status: status)
            try check(try await fresh().lookup(indexedSelection).source == "LRCLIB", "Optional source HTTP \(status) does not hide available line lyrics")
        }
        LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(indexedBody(), mime: "text/html")
        try check(try await fresh().lookup(indexedSelection).source == "LRCLIB", "Indexed HTML is rejected without losing line lyrics")
        LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(Data(repeating: 65, count: 512 * 1024 + 1))
        try check(try await fresh().lookup(indexedSelection).source == "LRCLIB", "Oversized indexed documents are bounded before XML parsing")
        LyricsHTTPReplies.shared.reset(404, Data()); indexedReply(indexedBody())
        try check(try await fresh().lookup(indexedSelection).source == "AMLL TTML DB", "Exact indexed word lyrics remain available when LRCLIB has no match")
        LyricsHTTPReplies.shared.reset(429, Data(), headers: ["Retry-After": "120"]); indexedReply(indexedBody())
        try check(try await fresh().lookup(indexedSelection).source == "AMLL TTML DB", "LRCLIB rate limiting does not block the independent indexed source")
        LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(Data(), status: 429, headers: ["Retry-After": "120"])
        let throttledIndex = fresh(); _ = try await throttledIndex.lookup(indexedSelection)
        let otherID = MediaItem(id: "spotify:track:BBBBBBBBBBBBBBBBBBBBBB", title: item.title, artists: item.artists, album: item.album,
                                durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities, location: nil)
        _ = try await throttledIndex.lookup(LyricsSelection(otherID))
        try check(LyricsHTTPReplies.shared.recorded().filter { $0.url?.host == "raw.githubusercontent.com" }.count == 1,
                  "Indexed Retry-After suppresses only its own requests across selections")
        LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(indexedBody(), delay: 4)
        let started = ContinuousClock.now
        try check(try await fresh().lookup(indexedSelection).source == "LRCLIB" && started.duration(to: .now) < .seconds(3.5),
                  "A slow optional word source releases available line lyrics within its two-second budget")
        let wordYAML = "version: '1.0'\nlines:\n - text: 'Our light'\n   start_ms: 2000\n   end_ms: 8100\n   words:\n    - {text: 'Our ', start_ms: 2000, end_ms: 4000}\n    - {text: light, start_ms: 4000, end_ms: 8100}\n"
        LyricsHTTPReplies.shared.reset(200, try response(yaml: wordYAML)); indexedReply(indexedBody(), delay: 4)
        let wordStarted = ContinuousClock.now
        try check(try await fresh().lookup(indexedSelection).source == "LRCLIB" && wordStarted.duration(to: .now) < .seconds(1.5),
                  "Existing LRCLIB word timing cancels a slow optional source without waiting for its budget")
        LyricsHTTPReplies.shared.reset(200, try response())
        LyricsHTTPReplies.shared.set(host: "lrclib.net", body: try response(), delay: 4); indexedReply(indexedBody(), delay: 4)
        let cancelledService = fresh(), cancelledLookup = Task { try await cancelledService.lookup(indexedSelection) }
        try await wait { LyricsHTTPReplies.shared.recorded().count == 2 }
        cancelledLookup.cancel()
        do { _ = try await cancelledLookup.value; try check(false, "Cancellation expected") } catch is CancellationError { }
        LyricsHTTPReplies.shared.reset(200, try response()); indexedReply(indexedBody())
        try check(try await cancelledService.lookup(indexedSelection).source == "AMLL TTML DB", "Cancelling both source requests releases admission for the latest selection")
        for id in ["spotify:episode:AAAAAAAAAAAAAAAAAAAAAA", "spotify:track:bad/../../id", "spotify:track:ＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡＡ", "local:track:AAAAAAAAAAAAAAAAAAAAAA"] {
            LyricsHTTPReplies.shared.reset(200, try response())
            let unsupported = MediaItem(id: id, title: item.title, artists: item.artists, album: item.album, durationMs: item.durationMs,
                                        artwork: nil, capabilities: item.capabilities, location: nil)
            _ = try await fresh().lookup(LyricsSelection(unsupported))
            try check(LyricsHTTPReplies.shared.recorded().count == 1 && LyricsHTTPReplies.shared.recorded()[0].url?.host == "lrclib.net",
                      "Only a valid ASCII Spotify track identity can enter the indexed lyrics path")
        }
        LyricsHTTPReplies.shared.reset(429, Data("Rate limited".utf8), headers: ["Content-Type": "text/plain", "Retry-After": "120"])
        let limited = fresh()
        do { _ = try await limited.lookup(selection); try check(false, "Rate limit expected") } catch LyricsFailure.rateLimited { }
        do { _ = try await limited.lookup(selection); try check(false, "Rate limit expected") } catch LyricsFailure.rateLimited { try check(LyricsHTTPReplies.shared.recorded().filter { $0.url?.host == "lrclib.net" }.count == 1, "Retry-After suppresses subsequent HTTP requests") }
        try check(LyricsService.retryDelay("120") == 120 && LyricsService.retryDelay("bad") == 30, "Retry-After parsing has a bounded fallback")
        let originalFile = directory.appendingPathComponent("original.lrc")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let originalLRC = "[00:01]Мой оригинальный текст\n[00:05]Домой"
        try Data(originalLRC.utf8).write(to: originalFile)
        let singleCreditItem = MediaItem(id: item.id, title: item.title, artists: [item.artists[0]], album: item.album,
                                         durationMs: item.durationMs, artwork: item.artwork, capabilities: item.capabilities)
        let importSelection = LyricsSelection(singleCreditItem)
        let imported = try await service.importFile(originalFile, for: importSelection)
        try check(imported.imported && imported.document.lines[0].text == "Мой оригинальный текст", "Explicit UTF-8 import overrides online lyrics for that recording")
        let encodedFile = directory.appendingPathComponent("encoded.lrc")
        let markedEncodings: [(String, String.Encoding, [UInt8])] = [
            ("UTF-8", .utf8, [0xEF, 0xBB, 0xBF]), ("UTF-16LE", .utf16LittleEndian, [0xFF, 0xFE]),
            ("UTF-16BE", .utf16BigEndian, [0xFE, 0xFF])
        ]
        for (name, encoding, bom) in markedEncodings {
            guard let payload = originalLRC.data(using: encoding) else {
                throw NSError(domain: "LyricsRegression cannot encode LRC fixture", code: 1)
            }
            let bytes = Data(bom) + payload
            try bytes.write(to: encodedFile)
            let encoded = try await service.importFile(encodedFile, for: importSelection)
            try check(encoded.document == imported.document && Data(contentsOf: encodedFile) == bytes,
                      "BOM-marked \(name) LRC preserves the first lyric, its timing, and source bytes")
        }
        try (Data(contentsOf: encodedFile) + Data([0x5B])).write(to: encodedFile)
        do { _ = try await service.importFile(encodedFile, for: importSelection); try check(false, "Truncated UTF-16 import should fail") }
        catch LyricsFailure.invalid { try check(true, "Truncated BOM-marked UTF-16 cannot overwrite a valid lyric import") }
        let reopened = LyricsService(directory: directory, session: session)
        let requestCountBeforeReopen = LyricsHTTPReplies.shared.recorded().count
        let restoredImport = try await reopened.lookup(selection)
        try check(restoredImport.imported && restoredImport.document == imported.document && LyricsHTTPReplies.shared.recorded().count == requestCountBeforeReopen,
                  "An import saved with a single artist survives restart and additional credits without network")
        let index = directory.appendingPathComponent("imports.json")
        let original = try Data(contentsOf: index)
        try Data("corrupt original".utf8).write(to: index)
        let corrupt = LyricsService(directory: directory, session: session)
        do { _ = try await corrupt.importFile(originalFile, for: selection); try check(false, "Corrupt index must be preserved") } catch LyricsFailure.storage { try check(try Data(contentsOf: index) == Data("corrupt original".utf8), "Corrupt import storage is preserved without reset or overwrite") }
        try original.write(to: index)
        let plainLRC = "[ti:Original untimed fixture]\nСвет на кухне\nТихо за окном"
        try Data(plainLRC.utf8).write(to: encodedFile)
        let untimedImport = try await fresh().importFile(encodedFile, for: importSelection)
        try check(untimedImport.imported && untimedImport.document.precision == "plain"
                  && untimedImport.document.lines.map(\.text) == ["Свет на кухне", "Тихо за окном"]
                  && untimedImport.document.lines.allSatisfy { $0.seekStartMs == nil },
                  "An untimed LRC import keeps its lines readable without synthetic seek points")
        let delayed = DelayedLyrics(); let store = LyricsStore(provider: delayed)
        store.adjustOffset(0.25)
        try check(store.offsetMilliseconds == 250 && store.playbackPosition(forLineStart: 1001) == 751
                  && store.playbackPosition(forLineStart: 100) == 0, "Earlier lyrics seek subtracts the correction in whole milliseconds and clamps preroll")
        store.adjustOffset(-0.5)
        try check(store.offsetMilliseconds == -250 && store.playbackPosition(forLineStart: 1001) == 1251,
                  "Later lyrics seek adds the correction without losing a millisecond")
        store.select(item); store.setVisible(true)
        try await wait { await delayed.ready(item.id) }
        let next = MediaItem(id: "original-second-item", title: "Another original recording", artists: item.artists, album: item.album, durationMs: item.durationMs, artwork: item.artwork, capabilities: item.capabilities, location: item.location)
        store.select(next); try await wait { await delayed.ready(next.id) }
        await delayed.resolve(item.id, result: fixture); await Task.yield()
        try check(store.selection?.id == next.id && store.result == nil && store.isLoading, "Late lyrics response cannot replace the next track")
        await delayed.resolve(next.id, result: fixture); try await wait { store.result != nil }
        store.adjustOffset(100); try check(store.offsetSeconds == 10, "User timing correction is bounded")
        store.select(item); try await wait { await delayed.ready(item.id) }; store.setVisible(false)
        await delayed.resolve(item.id, result: fixture); await Task.yield()
        try check(!store.isLoading && store.result == nil, "Closing lyrics rejects a late completion and clears loading")
        let finishing = FinishingLyrics(heldID: item.id, result: fixture)
        let following = LyricsStore(provider: finishing)
        following.select(item); following.setVisible(true)
        try await wait { await finishing.ready() }
        following.select(next)
        try await wait { await finishing.attempts[next.id, default: 0] > 0 }
        try check(following.isLoading && following.issue == nil, "The next track keeps loading while the cancelled lyrics request releases its slot")
        let latest = MediaItem(id: "original-latest-item", title: "Latest original recording", artists: item.artists, album: item.album,
                               durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities, location: item.location)
        following.select(latest)
        try await wait { await finishing.attempts[latest.id, default: 0] > 0 }
        let supersededAttempts = await finishing.attempts[next.id, default: 0]
        await finishing.finish()
        try await wait { following.result != nil }
        let finalSupersededAttempts = await finishing.attempts[next.id, default: 0]
        try check(following.selection?.id == latest.id && following.issue == nil && !following.isLoading
                  && finalSupersededAttempts == supersededAttempts,
                  "Rapid track changes resume only the latest lyrics lookup without a manual retry")
        let closingSource = FinishingLyrics(heldID: item.id, result: fixture)
        let closing = LyricsStore(provider: closingSource)
        closing.select(item); closing.setVisible(true); try await wait { await closingSource.ready() }
        closing.select(next); try await wait { await closingSource.attempts[next.id, default: 0] > 0 }
        closing.setVisible(false)
        let closedAttempts = await closingSource.attempts[next.id, default: 0]
        await closingSource.finish(); try await Task.sleep(for: .milliseconds(500))
        let finalClosedAttempts = await closingSource.attempts[next.id, default: 0]
        try check(!closing.isLoading && closing.result == nil && finalClosedAttempts == closedAttempts,
                  "Closing lyrics cancels a pending admission retry and prevents hidden lookups")
        closing.setVisible(true); try await wait { closing.result != nil }
        try check(closing.issue == nil && closing.selection?.id == next.id, "Reopening lyrics loads the latest selection after its predecessor finishes")
        let blockedSource = FailingLyrics(.busy), blocked = LyricsStore(provider: blockedSource)
        blocked.select(item); blocked.setVisible(true)
        try await wait(attempts: 400) { !blocked.isLoading }
        try check((await blockedSource.attempts) == 9 && blocked.issue == LyricsFailure.busy.errorDescription && blocked.result == nil,
                  "Persistent lyrics admission failure has a bounded retry budget instead of an endless spinner")
        let limitedSource = FailingLyrics(.rateLimited(120)), limitedStore = LyricsStore(provider: limitedSource)
        limitedStore.select(item); limitedStore.setVisible(true)
        try await wait { !limitedStore.isLoading }; try await Task.sleep(for: .milliseconds(100))
        try check((await limitedSource.attempts) == 1 && limitedStore.issue == LyricsFailure.rateLimited(120).errorDescription,
                  "Lyrics Retry-After is surfaced without an automatic network retry")
        let stagedSource = ProvisionalLyrics(), staged = LyricsStore(provider: stagedSource)
        let early = LyricsResult(document: try LyricsService.parse("[00:01]Early original lyric", format: "lrc", duration: item.durationMs),
                                 source: "Provisional fixture", sourceURL: nil, imported: false)
        staged.select(item); staged.setVisible(true)
        try await wait { await stagedSource.attempts == 1 }
        await stagedSource.publish(early, on: 1)
        let earlyRevision = staged.presentationRevision
        try check(staged.isLoading && staged.result == early && !staged.canTranslate,
                  "Validated provisional lyrics appear while better timing is still being fetched")
        await stagedSource.finish(.success(early), on: 1)
        try await wait { !staged.isLoading }
        try check(staged.presentationRevision == earlyRevision && staged.canTranslate,
                  "An equal final result does not replay the provisional lyric transition")

        staged.load()
        try await wait { await stagedSource.attempts == 2 }
        staged.select(next)
        try await wait { await stagedSource.attempts == 3 }
        await stagedSource.publish(early, on: 2)
        await stagedSource.finish(.success(early), on: 2)
        try check(staged.selection?.id == next.id && staged.result == nil && staged.isLoading,
                  "A previous track’s provisional callback cannot update the new song")
        await stagedSource.publish(early, on: 3)
        staged.setVisible(false)
        await stagedSource.finish(.success(fixture), on: 3)
        staged.setVisible(true)
        try await wait { await stagedSource.attempts == 4 }
        try check(staged.isLoading && staged.result == early,
                  "Reopening keeps visible provisional text and resumes the interrupted lookup")
        await stagedSource.finish(.failure(.network), on: 4)
        try await wait { !staged.isLoading }
        try check(staged.result == early && staged.issue == nil && staged.notice == nil,
                  "An optional lookup failure leaves usable provisional lyrics without an error overlay")
        let typography = OriginalLyricsFixture.typographyLines
        let rtl = LyricsTextLayout(); rtl.setLine(typography[0]); _ = rtl.measure(width: 360)
        let shape = rtl.words[1]
        let full = shape.revealed(at: 1), half = shape.revealed(at: 0.5)
        try check(!full.isEmpty && half.count == 1 && abs(half[0].maxX - full[0].maxX) < 0.01 && half[0].minX > full[0].minX && shape.segments.allSatisfy(\.rightToLeft), "Hebrew word sweep starts at the native right edge and advances left")
        let mixed = LyricsTextLayout(); mixed.setLine(typography[2]); let measured = mixed.measure(width: 300)
        try check(measured.height.isFinite && measured.height > 0 && mixed.words.count == 5 && mixed.words[0].segments.allSatisfy { !$0.rightToLeft } && mixed.words[1].segments.allSatisfy(\.rightToLeft) && !mixed.words[4].segments.isEmpty, "Mixed-script lyrics retain native bidi direction, combining marks and family emoji")
        let leading = mixed.words[0].revealed(at: 0.5), complete = mixed.words[0].revealed(at: 1)
        try check(!leading.isEmpty && abs(leading[0].minX - complete[0].minX) < 0.01 && leading[0].maxX < complete[0].maxX, "Latin word sweep retains its left-to-right direction inside mixed lyrics")
        let wrapped = LyricsTextLayout(); wrapped.setLine(typography[3]); let narrow = wrapped.measure(width: 160)
        let long = wrapped.words[0], middle = long.revealed(at: 0.5)
        let total = long.segments.reduce(CGFloat(0)) { $0 + $1.rect.width }
        let revealed = middle.reduce(CGFloat(0)) { $0 + $1.width }
        try check(long.segments.count > 1 && narrow.height > 34 && abs(revealed / total - 0.5) < 0.001 && middle[0].minY == long.segments[0].rect.minY, "A sustained word continues across wrapped rows within its original interval")
        try check(Set(long.segments.map { $0.rect.minY }).count >= 3 && long.segments.allSatisfy { $0.rect.height < narrow.height / 2 } && middle.count < long.segments.count, "Wrapped word masks remain separate per row instead of revealing a rectangular block across rows")
        let generation = wrapped.layoutGeneration
        _ = wrapped.measure(width: 160); _ = long.revealed(at: 0.75)
        try check(wrapped.layoutGeneration == generation && long.revealed(at: 0).isEmpty && long.revealed(at: .nan).isEmpty, "Clock-only lyrics updates reuse shaping and reject invalid sweep positions")
        _ = wrapped.measure(width: 600)
        try check(wrapped.layoutGeneration == generation + 1 && wrapped.size.height < narrow.height, "Resizing recomputes native wrapping without changing word timing")
        let sharedGlyphs = try LyricsService.parse("""
            <tt xmlns="http://www.w3.org/ns/ttml"><body><div>
              <p begin="0s" end="7s"><span begin="1s" end="2s">سل</span><span begin="3s" end="7s">ام</span></p>
              <p begin="8s" end="14s"><span begin="0s" end="1s">e</span><span begin="1s" end="5s">&#x301;</span><span begin="5s" end="6s">lan</span></p>
              <p begin="15s" end="21s"><span begin="0s" end="1s">👩</span><span begin="1s" end="3s">🏽</span><span begin="3s" end="6s">&#x200D;🚀</span></p>
            </div></body></tt>
            """, format: "ttml", duration: 21_000)
        let ligature = LyricsTextLayout(); ligature.setLine(sharedGlyphs.lines[0]); _ = ligature.measure(width: 360)
        try check(sharedGlyphs.precision == "word" && sharedGlyphs.lines[0].words.map(\.startMs) == [1000, 3000]
                  && ligature.words.map(\.intervals) == [[1000..<2000], [1000..<2000, 3000..<7000], [3000..<7000]],
                  "Only the shared Arabic ligature combines the two original TTML cues")
        let middleLigature = ligature.words[1]
        try check(ligature.words[0].progress(at: 2500) == 1 && ligature.words[2].progress(at: 2500) == 0
                  && abs(middleLigature.progress(at: 2500) - 0.2) < 0.001 && abs(middleLigature.progress(at: 5000) - 0.6) < 0.001
                  && middleLigature.segments.allSatisfy(\.rightToLeft),
                  "The shared RTL sweep holds through an untimed gap and leaves neighboring glyph clocks independent")
        let combining = LyricsTextLayout(); combining.setLine(sharedGlyphs.lines[1]); _ = combining.measure(width: 360)
        try check(combining.words.map(\.intervals) == [[8000..<13_000], [13_000..<14_000]]
                  && abs(combining.words[0].progress(at: 11_000) - 0.6) < 0.001,
                  "A timed combining mark and its base produce one continuing native sweep")
        let emoji = LyricsTextLayout(); emoji.setLine(sharedGlyphs.lines[2]); _ = emoji.measure(width: 360)
        try check(sharedGlyphs.lines[2].words.count == 3 && emoji.words.map(\.intervals) == [[15_000..<21_000]]
                  && emoji.words[0].progress(at: 18_000) == 0.5,
                  "The whole timed ZWJ emoji uses one shared shape and retains the source cues")
        let accented = try LyricsService.parse("""
            <tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1s" end="9s">
              <span begin="0s" end="6s">तुम्हारी आवाज़</span>
            </p></div></body></tt>
            """, format: "ttml", duration: 9000)
        let overhang = LyricsTextLayout(); overhang.setLine(accented.lines[0]); let overhangSize = overhang.measure(width: 160)
        let partialInk = try lyricRaster(overhang, size: overhangSize, milliseconds: 4000, phase: .sweeping)
        let finishedInk = try lyricRaster(overhang, size: overhangSize, milliseconds: 8000, phase: .sweeping)
        let fullInk = try lyricRaster(overhang, size: overhangSize, milliseconds: 9000, phase: .sung)
        try check(unpaintedInk(in: partialInk, comparedTo: fullInk) > 0 && unpaintedInk(in: finishedInk, comparedTo: fullInk) == 0,
                  "A completed cue paints all shaped ink, including overhangs, before the whole line ends")
        let forcedBreak = """
            <tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="0s" end="7s">
              <span begin="0s" end="2s">River</span><br/><span begin="2s" end="7s">sing<br/>ing</span>
            </p></div></body></tt>
            """
        for format in ["ttml", "ttml-amll"] {
            let parsed = try LyricsService.parse(forcedBreak, format: format, duration: 7000)
            let line = parsed.lines[0]
            let renderer = LyricsTextLayout(); renderer.setLine(line); _ = renderer.measure(width: 360)
            let first = renderer.words.first?.segments.first, held = renderer.words.last
            try check(parsed.precision == "word" && line.text == "River\nsing\ning" && renderer.words.count == 2
                      && held?.intervals == [2000..<7000] && held?.progress(at: 4500) == 0.5
                      && held?.segments.count == 2 && (held?.segments.first?.rect.minY ?? 0) > (first?.rect.minY ?? 0)
                      && (held?.segments.first?.rect.width ?? 360) < 180 && held?.revealed(at: 0.75).count == 2,
                      "\(format): a sustained cue sweeps both forced TextKit rows without filling the blank margin")
        }
        let polyphony = try LyricsService.parse(OriginalLyricsFixture.overlappingVoices, format: "ttml-amll", duration: 12_000)
        guard let lead = polyphony.lines.first, let backing = lead.backingLines.first,
              case .ready(let voiceInputs) = polyphony.translationInput() else {
            throw NSError(domain: "LyricsRegression", code: 1, userInfo: [NSLocalizedDescriptionKey: "Missing AMLL vocal tracks"])
        }
        try check(polyphony.isValid && polyphony.precision == "word" && lead.text == "Hold the light" && backing.text == "Soft echo"
                  && lead.endMs == 8000 && backing.endMs == 9000 && polyphony.focusedLine(at: 8500) == 0
                  && polyphony.focusedLine(at: 9500) == 1,
                  "The Rust ABI decodes independent AMLL voices and keeps focus during the backing tail")
        let backingPreroll = try LyricsService.parse("""
            <tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata">
              <body dur="12s"><div>
                <p begin="1s" end="8s"><span begin="1s" end="2s">Carry </span><span begin="2s" end="8s">the light</span></p>
                <p begin="8s" end="10s">
                  <span ttm:role="x-bg" begin="3s" end="6s"><span begin="3s" end="6s">Soft echo</span></span>
                  <span begin="8s" end="10s">Meet the day</span>
                </p>
              </div></body>
            </tt>
            """, format: "ttml-amll", duration: 12_000)
        try check([2500.0, 4000, 6500, 8500].map { backingPreroll.focusedLine(at: $0) } == [0, 0, 0, 1]
                  && backingPreroll.focusedLine(at: 4000, paused: true) == 0
                  && backingPreroll.lines[1].backingLines.first?.contains(4000) == true,
                  "A later group's early backing voice sweeps without taking focus from the active lead")
        let leadShape = LyricsTextLayout(), backingShape = LyricsTextLayout()
        leadShape.setLine(lead); let leadHeight = leadShape.measure(width: 360).height
        backingShape.setLine(backing, fontSize: 19); let backingHeight = backingShape.measure(width: 360).height
        try check(leadHeight > backingHeight && leadShape.words.last?.intervals == [5000..<8000]
                  && backingShape.words.first?.intervals == [4500..<6000]
                  && abs((leadShape.words.last?.progress(at: 5500) ?? 0) - 1.0 / 6) < 0.001
                  && abs((backingShape.words.first?.progress(at: 5500) ?? 0) - 2.0 / 3) < 0.001,
                  "Lead and backing native glyphs sweep independently at the same playback position")
        let voiceGroups = LyricsTranslationGroup.contextual(voiceInputs)
        let translation = polyphony.applyingTranslation([0: "Garde la lumière", 1: "Puis laisse entrer le jour", 2: "Doux écho"], to: voiceInputs)
        try check(voiceGroups.map { $0.lines.map(\.index) } == [[0, 1], [2]]
                  && translation?.lines[0].text == "Garde la lumière" && translation?.lines[0].words.isEmpty == true
                  && translation?.lines[0].backingLines[0].text == "Doux écho"
                  && translation?.lines[0].backingLines[0].startMs == 4500 && translation?.focusedLine(at: 8500) == 0,
                  "Translation preserves both voice intervals and excludes backing lyrics from lead phrase context")
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 200, height: 200), styleMask: .borderless, backing: .buffered, defer: false)
        let other = NSWindow(contentRect: .zero, styleMask: .borderless, backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false; other.isReleasedWhenClosed = false
        let parent = NSView(frame: NSRect(x: 0, y: 0, width: 200, height: 200))
        let observer = LyricsScrollObservationView(frame: NSRect(x: 20, y: 20, width: 100, height: 100))
        var scrolls = 0; observer.onScroll = { scrolls += 1; store.followsPlayback = false }
        parent.addSubview(observer); window.contentView = parent; window.orderFront(nil)
        defer { observer.stop(); window.orderOut(nil); window.close(); other.close() }
        store.followsPlayback = true
        observer.observe(window: other, point: NSPoint(x: 40, y: 40), deltaX: 0, deltaY: 10)
        observer.observe(window: window, point: NSPoint(x: 160, y: 160), deltaX: 0, deltaY: 10)
        observer.observe(window: window, point: NSPoint(x: 40, y: 40), deltaX: 0, deltaY: 0)
        try check(scrolls == 0 && store.followsPlayback, "Scrolling outside the lyric region or in another window preserves Follow")
        observer.observe(window: window, point: NSPoint(x: 40, y: 40), deltaX: 0, deltaY: 10)
        try check(scrolls == 1 && !store.followsPlayback, "A wheel or trackpad scroll inside lyrics suspends automatic following")
        observer.isHidden = true
        observer.observe(window: window, point: NSPoint(x: 40, y: 40), deltaX: 0, deltaY: 10)
        observer.isHidden = false; observer.removeFromSuperview()
        observer.observe(window: window, point: NSPoint(x: 40, y: 40), deltaX: 0, deltaY: 10)
        try check(scrolls == 1, "Hidden and detached lyric observers cannot alter Follow")
        return passed
    }

    private static func checkMusixmatch(item: MediaItem, directory: URL, session: URLSession,
                                        primaryReply: Data, primaryDocument: LyricsDocument,
                                        check: (Bool, String) throws -> Void) async throws {
        let host = "apic-appmobile.musixmatch.com"
        let tokenPath = "/ws/1.1/token.get"
        let macroPath = "/ws/1.1/macro.subtitles.get"
        let fixtureToken = "fixture-anonymous-token"
        let providerTrackID = "AAAAAAAAAAAAAAAAAAAAAA"
        let recordingID = "spotify:track:\(providerTrackID)"
        let lineBody = #"[{"text":"Original line timing","time":{"total":1.25}}]"#
        let wordBody = #"[{"ts":1,"te":5,"l":[{"c":"Carry ","o":0},{"c":"the light","o":1}]}]"#
        let replies = LyricsHTTPReplies.shared

        func selection(id: String) -> LyricsSelection {
            LyricsSelection(MediaItem(id: id, title: item.title, artists: item.artists, album: item.album,
                                      durationMs: item.durationMs, artwork: nil, capabilities: item.capabilities))
        }
        func service(enabled: Bool = true) -> LyricsService {
            LyricsService(directory: directory.appendingPathComponent(UUID().uuidString), session: session,
                          enableUnison: false, lrcRedContact: nil, enableMusixmatch: enabled)
        }
        func envelope(_ body: [String: Any]) -> [String: Any] {
            ["message": ["header": ["status_code": 200], "body": body]]
        }
        func fixture(richsync: String = wordBody, trackLengthDelta: Int = 0, restricted: Bool = false,
                     returnedID: String = providerTrackID, delay: TimeInterval = 0) throws {
            let trackLength = Int((Double(item.durationMs) / 1000).rounded()) + trackLengthDelta
            let track: [String: Any] = [
                "track_id": 100, "commontrack_id": 200, "track_name": item.title,
                "artist_name": item.artists[0], "album_name": item.album ?? "",
                "track_length": trackLength, "track_spotify_id": returnedID, "restricted": 0, "instrumental": 0
            ]
            let macro = envelope(["macro_calls": [
                "matcher.track.get": envelope(["track": track]),
                "track.richsync.get": envelope(["richsync": ["richsync_body": richsync, "restricted": 0]]),
                "track.subtitles.get": envelope(["subtitle_list": [["subtitle": ["subtitle_body": lineBody, "restricted": 0]]]]),
                "track.lyrics.get": envelope(["lyrics": ["lyrics_body": "Original fixture text", "restricted": restricted ? 1 : 0]])
            ]])
            replies.set(path: tokenPath, body: try JSONSerialization.data(withJSONObject: envelope(["user_token": fixtureToken])))
            replies.set(path: macroPath, body: try JSONSerialization.data(withJSONObject: macro), delay: delay)
        }
        func providerRequests() -> [URLRequest] {
            replies.recorded().filter { $0.url?.host == host }
        }
        func query(_ request: URLRequest?, _ name: String) -> String? {
            guard let url = request?.url else { return nil }
            return URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == name }?.value
        }

        let selected = selection(id: recordingID)
        replies.reset(200, primaryReply)
        try fixture()
        try check(try await service(enabled: false).lookup(selected).document == primaryDocument && providerRequests().isEmpty,
                  "Disabled Musixmatch never starts an anonymous client session")

        replies.reset(200, primaryReply)
        try fixture()
        let enabled = service()
        let wordResult = try await enabled.lookup(selected)
        let words = wordResult.document.lines.first?.words ?? []
        try check(wordResult.source == "Musixmatch" && wordResult.document.precision == "word"
                  && words.map(\.startMs) == [1000, 2000] && words.map(\.endMs) == [2000, 5000]
                  && words.allSatisfy(\.endEstimated),
                  "Musixmatch richsync keeps absolute line clocks and derives word ends from the next source mark")
        let initialRequests = providerRequests()
        try check(initialRequests.compactMap { $0.url?.path } == [tokenPath, macroPath]
                  && query(initialRequests.first, "app_id") == "mac-ios-v2.0"
                  && query(initialRequests.last, "usertoken") == fixtureToken
                  && query(initialRequests.last, "track_spotify_id") == recordingID
                  && query(initialRequests.last, "optional_calls") == "track.richsync"
                  && initialRequests.allSatisfy { $0.value(forHTTPHeaderField: "Authorization") == nil && $0.value(forHTTPHeaderField: "Cookie") == nil }
                  && replies.recorded().filter { $0.url?.host != host }.allSatisfy { query($0, "usertoken") == nil },
                  "The anonymous client token is used only on the Musixmatch host, with no account cookies or authorization")

        try fixture(richsync: "{")
        let lineResult = try await enabled.lookup(selection(id: "musixmatch-fixture-local-copy"))
        try check(lineResult.source == "Musixmatch" && lineResult.document.precision == "line"
                  && lineResult.document.lines.first?.startMs == 1250
                  && providerRequests().compactMap { $0.url?.path } == [tokenPath, macroPath, macroPath],
                  "Malformed richsync falls back to actual line cues and a second lookup reuses the anonymous token")

        let rejected = [
            (lengthDelta: 8, restricted: false, id: providerTrackID, check: "Musixmatch rejects a different recording duration"),
            (lengthDelta: 0, restricted: false, id: "BBBBBBBBBBBBBBBBBBBBBB", check: "Musixmatch rejects a different Spotify track ID even with identical metadata"),
            (lengthDelta: 0, restricted: true, id: providerTrackID, check: "Restricted Musixmatch lyrics cannot replace the existing text")
        ]
        for scenario in rejected {
            replies.reset(200, primaryReply)
            try fixture(trackLengthDelta: scenario.lengthDelta, restricted: scenario.restricted, returnedID: scenario.id)
            try check(try await service().lookup(selected).document == primaryDocument, scenario.check)
        }

        replies.reset(200, primaryReply)
        replies.set(path: tokenPath, status: 429, body: Data(), headers: ["Retry-After": "120"])
        let limited = service()
        let beforeThrottle = try await limited.lookup(selected)
        let afterThrottle = try await limited.lookup(selection(id: "musixmatch-fixture-after-throttle"))
        try check(beforeThrottle.document == primaryDocument && afterThrottle.document == primaryDocument
                  && providerRequests().compactMap { $0.url?.path } == [tokenPath],
                  "Musixmatch Retry-After suppresses further provider requests while primary lyrics remain available")

        replies.reset(200, primaryReply)
        try fixture(delay: 4)
        let cancellable = service()
        let pending = Task { try await cancellable.lookup(selected) }
        for _ in 0..<100 {
            if providerRequests().contains(where: { $0.url?.path == macroPath }) { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        let macroStarted = providerRequests().contains { $0.url?.path == macroPath }
        pending.cancel()
        do {
            _ = try await pending.value
            try check(false, "Cancelled Musixmatch request cannot return lyrics")
        } catch is CancellationError {
            try check(macroStarted, "Cancelling during Musixmatch lookup propagates cancellation")
        }
    }

    private static func lyricRaster(_ renderer: LyricsTextLayout, size: CGSize, milliseconds: Double, phase: LyricsPaintPhase) throws -> NSBitmapImageRep {
        let scale = 2
        let width = Int(size.width) * scale, height = Int(size.height) * scale
        guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
                                            bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                            colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
              let context = NSGraphicsContext(bitmapImageRep: bitmap)?.cgContext else {
            throw NSError(domain: "LyricsRegression raster", code: 1)
        }
        context.setFillColor(NSColor.black.cgColor)
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        context.translateBy(x: 0, y: CGFloat(height))
        context.scaleBy(x: CGFloat(scale), y: -CGFloat(scale))
        NSGraphicsContext.saveGraphicsState()
        defer { NSGraphicsContext.restoreGraphicsState() }
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: true)
        renderer.draw(milliseconds: milliseconds, phase: phase, reduceMotion: true)
        return bitmap
    }

    private static func unpaintedInk(in bitmap: NSBitmapImageRep, comparedTo full: NSBitmapImageRep) -> Int {
        var pixels = 0
        for y in 0..<full.pixelsHigh {
            for x in 0..<full.pixelsWide {
                let expected = full.colorAt(x: x, y: y)?.redComponent ?? 0
                let actual = bitmap.colorAt(x: x, y: y)?.redComponent ?? 0
                if expected > 0.8, expected - actual > 0.2 { pixels += 1 }
            }
        }
        return pixels
    }
}
