import Foundation
import OSLog

private final class NoLyricsRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}

struct LrcRedContact: Sendable {
    private let projectURL: URL
    private let email: String

    init?(projectURL: String?, email: String?) {
        guard let projectURL, let url = URLComponents(string: projectURL), url.scheme == "https",
              let host = url.host, host.contains("."), url.user == nil, url.password == nil,
              url.query == nil, url.fragment == nil, projectURL.utf8.count <= 256,
              let normalizedURL = url.url, let email, email.utf8.count <= 254,
              email.utf8.allSatisfy({ (33...126).contains($0) }),
              email.split(separator: "@", omittingEmptySubsequences: false).count == 2,
              let at = email.firstIndex(of: "@"), at != email.startIndex,
              email[email.index(after: at)...].contains(".") else { return nil }
        self.projectURL = normalizedURL
        self.email = email
    }

    static func configured() -> Self? {
        let defaults = UserDefaults.standard
        let contact = Self(projectURL: defaults.string(forKey: "LyricsProjectURL"), email: defaults.string(forKey: "LyricsContactEmail"))
        guard let contact, let host = contact.projectURL.host()?.lowercased(),
              !["example.com", "example.net", "example.org"].contains(where: { host == $0 || host.hasSuffix("." + $0) }) else { return nil }
        return contact
    }

    func userAgent(app: String) -> String { "\(app) +\(projectURL.absoluteString) \(email)" }
}

/// Metadata only; timing and untrusted XML/DTD validation belong to the Rust parser.
private final class IndexedLyricsMetadata: NSObject, XMLParserDelegate {
    private var path: [String] = []
    private var values: [String: [String]] = [:]
    private var valid = true
    func parser(_ parser: XMLParser, didStartElement elementName: String, namespaceURI: String?, qualifiedName qName: String?, attributes attributeDict: [String: String]) {
        path.append(elementName)
        guard path.count <= 64 else { valid = false; parser.abortParsing(); return }
        guard path == ["tt", "head", "metadata", "meta"], namespaceURI == "http://www.example.com/ns/amll",
              let key = attributeDict["key"], ["spotifyId", "musicName", "artists", "album"].contains(key) else { return }
        guard let value = attributeDict["value"], value.utf8.count <= 1024, values[key, default: []].count < 32 else {
            valid = false; parser.abortParsing(); return
        }
        values[key, default: []].append(value)
    }
    func parser(_ parser: XMLParser, didEndElement elementName: String, namespaceURI: String?, qualifiedName qName: String?) { path.removeLast() }
    static func matches(_ data: Data, id: String, selection: LyricsSelection) -> Bool {
        let delegate = IndexedLyricsMetadata(), parser = XMLParser(data: data)
        parser.shouldProcessNamespaces = true; parser.shouldResolveExternalEntities = false; parser.delegate = delegate
        guard parser.parse(), delegate.valid else { return false }
        let contains = { (key: String, expected: String) in delegate.values[key, default: []].contains { LyricsService.canonical($0) == LyricsService.canonical(expected) } }
        let creditedArtists = [selection.artist] + (selection.additionalArtists ?? [])
        return delegate.values["spotifyId", default: []].contains(id)
            && contains("musicName", selection.title) && creditedArtists.contains { contains("artists", $0) }
            && (selection.album.isEmpty || contains("album", selection.album))
    }
}

/// Separate IO/parser owner. No engine handle, account credentials or PCM enters this service.
actor LyricsService: LyricsProviding {
    static let musixmatchPreferenceKey = "AuvraExperimentalMusixmatchLyrics"
    private static let log = Logger(subsystem: "app.auvra.player", category: "Lyrics")
    private enum Outcome: Sendable { case result(LyricsResult), failure(LyricsFailure), unavailable }
    private struct Record: Codable {
        let selection: LyricsSelection
        let result: LyricsResult
        var expiresAt: Date? = nil
    }
    private struct Reply: Decodable {
        let id: Int64
        let instrumental: Bool
        let trackName, artistName: String
        let albumName: String?
        let duration: Double?
        let plainLyrics, syncedLyrics, lyricsfile: String?

        func durationDifference(for selection: LyricsSelection) -> Double {
            guard let duration, duration.isFinite else { return .infinity }
            return abs(duration * 1000 - Double(selection.durationMs))
        }

        func matches(_ selection: LyricsSelection, artists: Set<String>) -> Bool {
            id > 0 && durationDifference(for: selection) <= 2100
                && LyricsService.canonical(trackName) == LyricsService.canonical(selection.title)
                && artists.contains(LyricsService.canonical(artistName))
        }
    }
    private struct SearchReply: Decodable {
        let track: Reply?
        init(from decoder: any Decoder) { track = try? Reply(from: decoder) }
    }
    private let session: URLSession
    private let unison: UnisonLyrics?
    private let lrcRed: LrcRedLyrics?
    private let musixmatchOverride: Bool?
    private var musixmatch: MusixmatchLyrics?
    private let directory: URL
    private let userAgent: String
    private var cache: [Record] = []
    private var imports: [Record] = []
    private var loaded = false
    private var active = false
    private var nextRequest = Date.distantPast
    private var retryAfter = Date.distantPast
    private var indexedRetryAfter = Date.distantPast
    init(directory: URL? = nil, session: URLSession? = nil, userAgent: String? = nil, enableUnison: Bool = true,
         lrcRedContact: LrcRedContact? = LrcRedContact.configured(), enableMusixmatch: Bool? = nil) {
        self.directory = directory ?? URL.applicationSupportDirectory.appendingPathComponent("Auvra/Lyrics", isDirectory: true)
        let appAgent = userAgent ?? "Auvra/0.1.0 (macOS; app.auvra.player)"
        let transport: URLSession
        if let session { transport = session }
        else {
            let config = URLSessionConfiguration.ephemeral
            config.urlCache = nil
            config.httpCookieStorage = nil
            config.httpShouldSetCookies = false
            config.urlCredentialStorage = nil
            config.timeoutIntervalForRequest = 12; config.timeoutIntervalForResource = 15
            transport = URLSession(configuration: config, delegate: NoLyricsRedirects(), delegateQueue: nil)
        }
        self.session = transport
        self.userAgent = appAgent
        unison = enableUnison ? UnisonLyrics(session: transport, userAgent: appAgent) : nil
        lrcRed = lrcRedContact.map { LrcRedLyrics(session: transport, userAgent: $0.userAgent(app: appAgent)) }
        musixmatchOverride = enableMusixmatch
    }
    private func loadCache() throws {
        guard !loaded else { return }
        let file = directory.appendingPathComponent("imports.json")
        if FileManager.default.fileExists(atPath: file.path) {
            let data = try read(file, limit: 4 * 1024 * 1024)
            let records = try JSONDecoder().decode([Record].self, from: data)
            guard records.count <= 32 else { throw LyricsFailure.storage }
            guard records.allSatisfy({ $0.result.document.isValid && $0.result.imported }) else { throw LyricsFailure.storage }
            imports = records
        }
        loaded = true
    }
    private func remember(_ result: LyricsResult, for selection: LyricsSelection) {
        let expiresAt = result.document.precision == "plain" && !result.document.instrumental && !result.imported
            ? Date().addingTimeInterval(120) : nil
        cache.removeAll { $0.selection == selection }
        cache.append(Record(selection: selection, result: result, expiresAt: expiresAt))
        while cache.count > 32 || (try? JSONEncoder().encode(cache).count).map({ $0 > 4 * 1024 * 1024 }) == true { cache.removeFirst() }
    }
    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult {
        try await resolve(selection, provisional: nil)
    }
    func lookup(_ selection: LyricsSelection, provisional: @escaping LyricsProvisionalHandler) async throws -> LyricsResult {
        try await resolve(selection, provisional: provisional)
    }
    private func resolve(_ selection: LyricsSelection, provisional: LyricsProvisionalHandler?) async throws -> LyricsResult {
        // A corrupt import index is preserved and reported; never silently overwrite it.
        do { try loadCache() } catch { throw LyricsFailure.storage }
        if let match = imports.last(where: { $0.selection.sameRecording(as: selection) }) { return match.result }
        let useMusixmatch = musixmatchOverride ?? UserDefaults.standard.bool(forKey: Self.musixmatchPreferenceKey)
        if useMusixmatch != (musixmatch != nil) {
            guard !active else { throw LyricsFailure.busy }
            musixmatch = useMusixmatch ? MusixmatchLyrics(session: session) : nil
            cache.removeAll()
        }
        let now = Date()
        if let match = cache.last(where: { $0.selection == selection && ($0.expiresAt ?? .distantFuture) > now }) {
            return match.result
        }
        guard !active else { throw LyricsFailure.busy }
        guard !selection.title.isEmpty, !selection.artist.isEmpty, (1000...3_600_000).contains(selection.durationMs), selection.title.utf8.count <= 512, selection.artist.utf8.count <= 512, selection.album.utf8.count <= 1024 else { throw LyricsFailure.missing }
        active = true; defer { active = false; nextRequest = Date().addingTimeInterval(0.35) }
        if nextRequest > Date() { try await Task.sleep(for: .seconds(nextRequest.timeIntervalSinceNow)) }
        try Task.checkCancellation()
        let result: LyricsResult
        if let id = Self.indexedID(selection.id) {
            result = try await withThrowingTaskGroup(of: Outcome.self) { group in
                group.addTask {
                    do { return .result(try await self.lrclib(selection, provisional: provisional, awaitingIndexed: true)) }
                    catch is CancellationError { throw CancellationError() }
                    catch let error as LyricsFailure { return .failure(error) }
                    catch { return .failure(.network) }
                }
                group.addTask {
                    if let result = try await self.indexed(selection, id: id) { return .result(result) }
                    return .unavailable
                }
                defer { group.cancelAll() }
                var fallback: LyricsResult?, failure = LyricsFailure.missing
                for try await outcome in group {
                    try Task.checkCancellation()
                    switch outcome {
                    case .result(let result):
                        if result.document.precision == "word" { return result }
                        fallback = result
                    case .failure(let error): failure = error
                    case .unavailable: break
                    }
                }
                if let fallback { return fallback }
                throw failure
            }
        } else { result = try await lrclib(selection, provisional: provisional, awaitingIndexed: false) }
        try Task.checkCancellation()
        Self.log.debug("Selected \(result.source, privacy: .public) lyrics with \(result.document.precision, privacy: .public) precision and \(result.document.lines.count, privacy: .public) rows")
        remember(result, for: selection); return result
    }
    private static func indexedID(_ id: String) -> String? {
        let parts = id.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "spotify", parts[1] == "track", parts[2].utf8.count == 22,
              parts[2].utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) }) else { return nil }
        return String(parts[2])
    }
    private func indexed(_ selection: LyricsSelection, id: String) async throws -> LyricsResult? {
        guard Date() >= indexedRetryAfter else { return nil }
        // A slow optional source cannot hold line/plain lyrics behind a full network timeout.
        return try await withThrowingTaskGroup(of: LyricsResult?.self) { group in
            group.addTask { try await self.fetchIndexed(selection, id: id) }
            group.addTask { try await Task.sleep(for: .seconds(2)); return nil }
            defer { group.cancelAll() }
            return try await group.next() ?? nil
        }
    }
    private func fetchIndexed(_ selection: LyricsSelection, id: String) async throws -> LyricsResult? {
        let path = "/amll-dev/amll-ttml-db/refs/heads/main/spotify-lyrics/\(id).ttml"
        let url = URL(string: "https://raw.githubusercontent.com\(path)")!
        var request = URLRequest(url: url); request.timeoutInterval = 2
        request.setValue(userAgent, forHTTPHeaderField: "User-Agent")
        request.setValue("application/ttml+xml, application/xml, text/plain", forHTTPHeaderField: "Accept")
        do {
            let (bytes, response) = try await session.bytes(for: request)
            guard let http = response as? HTTPURLResponse, response.url == url else { return nil }
            Self.log.debug("AMLL TTML HTTP \(http.statusCode, privacy: .public)")
            if http.statusCode == 429 || http.statusCode == 503 {
                indexedRetryAfter = Date().addingTimeInterval(Self.retryDelay(http.value(forHTTPHeaderField: "Retry-After"))); return nil
            }
            guard http.statusCode == 200, ["application/ttml+xml", "application/xml", "text/xml", "text/plain"].contains(http.mimeType ?? ""), response.expectedContentLength <= 512 * 1024 else { return nil }
            var data = Data()
            for try await byte in bytes {
                try Task.checkCancellation()
                guard data.count < 512 * 1024 else { return nil }
                data.append(byte)
            }
            guard let text = String(data: data, encoding: .utf8) else { return nil }
            let document = try Self.parse(text, format: "ttml-amll", duration: selection.durationMs)
            guard document.precision == "word", document.isValid,
                  IndexedLyricsMetadata.matches(data, id: id, selection: selection) else { return nil }
            try Task.checkCancellation()
            return LyricsResult(document: document, source: "AMLL TTML DB", sourceURL: URL(string: "https://github.com/amll-dev/amll-ttml-db/blob/main/spotify-lyrics/\(id).ttml"), imported: false)
        } catch is CancellationError { throw CancellationError() }
        catch { if Task.isCancelled { throw CancellationError() }; return nil }
    }
    private func lrclib(_ selection: LyricsSelection, provisional: LyricsProvisionalHandler?, awaitingIndexed: Bool) async throws -> LyricsResult {
        let credits = [selection.artist] + (selection.additionalArtists ?? [])
        var artists: Set<String> = [Self.canonical(selection.artist)]
        if credits.count > 1 {
            for separator in [", ", " & ", " / ", " feat. ", " featuring ", " x ", " and ", "; "] {
                artists.insert(Self.canonical(credits.joined(separator: separator)))
            }
        }
        let searchQuery = [
            URLQueryItem(name: "track_name", value: selection.title),
            URLQueryItem(name: "artist_name", value: selection.artist)
        ]
        var exactQuery = searchQuery + [URLQueryItem(name: "duration", value: String(Double(selection.durationMs) / 1000))]
        if !selection.album.isEmpty {
            exactQuery.append(URLQueryItem(name: "album_name", value: selection.album))
        }

        let exact: LyricsResult?
        var primaryFailure = LyricsFailure.missing
        var searchAllowed = true
        do {
            let reply: Reply = try await fetchLRCLIB(path: "/api/get", query: exactQuery, timeout: 12)
            exact = reply.matches(selection, artists: artists) ? Self.lyricsResult(reply, duration: selection.durationMs) : nil
        } catch let error as LyricsFailure {
            exact = nil
            primaryFailure = error
            switch error {
            case .missing: break
            default: searchAllowed = false
            }
        }
        if let exact, exact.document.hasTiming || exact.document.instrumental {
            if awaitingIndexed { await publishProvisional(exact, to: provisional) }
            return exact
        }
        return try await fallbackLyrics(selection, exact: exact, failure: primaryFailure,
                                        searchQuery: searchAllowed ? searchQuery : nil, artists: artists, provisional: provisional)
    }

    private func fallbackLyrics(_ selection: LyricsSelection, exact: LyricsResult?, failure: LyricsFailure,
                                searchQuery: [URLQueryItem]?, artists: Set<String>,
                                provisional: LyricsProvisionalHandler?) async throws -> LyricsResult {
        try await withThrowingTaskGroup(of: Outcome.self) { group in
            if let searchQuery {
                group.addTask {
                    do {
                        // LRCLIB asks clients to space consecutive requests.
                        try await Task.sleep(for: .milliseconds(250))
                        guard let result = try await self.searchLRCLIB(selection, query: searchQuery, artists: artists) else { return .unavailable }
                        return .result(result)
                    } catch let error as LyricsFailure { return .failure(error) }
                }
            }
            if let unison {
                group.addTask {
                    guard let result = try await unison.lookup(selection) else { return .unavailable }
                    return .result(result)
                }
            }
            if let lrcRed {
                group.addTask {
                    guard let result = try await lrcRed.lookup(selection) else { return .unavailable }
                    return .result(result)
                }
            }
            if let musixmatch {
                group.addTask {
                    guard let result = try await musixmatch.lookup(selection) else { return .unavailable }
                    return .result(result)
                }
            }
            defer { group.cancelAll() }
            var best = exact
            var lastFailure = failure
            if let exact { await publishProvisional(exact, to: provisional) }
            for try await outcome in group {
                try Task.checkCancellation()
                switch outcome {
                case .result(let result):
                    if result.document.precision == "word" { return result }
                    let rank = Self.precision(of: result.document)
                    let bestRank = Self.precision(of: best?.document)
                    if rank > bestRank || (rank == bestRank && result.source == "LRCLIB" && best?.source != "LRCLIB") {
                        best = result
                        if rank > bestRank { await publishProvisional(result, to: provisional) }
                    }
                case .failure(let error): lastFailure = error
                case .unavailable: break
                }
            }
            if let best { return best }
            throw lastFailure
        }
    }

    private func publishProvisional(_ result: LyricsResult, to handler: LyricsProvisionalHandler?) async {
        guard let handler, !Task.isCancelled, !result.document.instrumental,
              result.document.precision == "plain" || result.document.precision == "line" else { return }
        await MainActor.run {
            guard !Task.isCancelled else { return }
            handler(result)
        }
    }

    private func searchLRCLIB(_ selection: LyricsSelection, query: [URLQueryItem], artists: Set<String>) async throws -> LyricsResult? {
        try await withThrowingTaskGroup(of: LyricsResult?.self) { group in
            group.addTask {
                let replies: [SearchReply] = try await self.fetchLRCLIB(path: "/api/search", query: query, timeout: 2)
                let album = Self.canonical(selection.album)
                let candidates = replies.compactMap(\.track).filter { !$0.instrumental && $0.matches(selection, artists: artists) }.sorted {
                    let firstOnAlbum = !album.isEmpty && Self.canonical($0.albumName ?? "") == album
                    let secondOnAlbum = !album.isEmpty && Self.canonical($1.albumName ?? "") == album
                    if firstOnAlbum != secondOnAlbum { return firstOnAlbum }
                    let firstDifference = $0.durationDifference(for: selection)
                    let secondDifference = $1.durationDifference(for: selection)
                    return firstDifference == secondDifference ? $0.id < $1.id : firstDifference < secondDifference
                }
                var best: LyricsResult?
                for candidate in candidates {
                    try Task.checkCancellation()
                    guard let result = Self.lyricsResult(candidate, duration: selection.durationMs) else { continue }
                    if result.document.precision == "word" { return result }
                    if Self.precision(of: result.document) > Self.precision(of: best?.document) { best = result }
                }
                return best
            }
            group.addTask {
                try await Task.sleep(for: .seconds(2))
                throw LyricsFailure.network
            }
            defer { group.cancelAll() }
            return try await group.next() ?? nil
        }
    }

    private func fetchLRCLIB<T: Decodable>(path: String, query: [URLQueryItem], timeout: TimeInterval) async throws -> T {
        guard Date() >= retryAfter else { throw LyricsFailure.rateLimited(max(1, Int(ceil(retryAfter.timeIntervalSinceNow)))) }
        var components = URLComponents(string: "https://lrclib.net")!
        components.path = path
        components.queryItems = query
        // The server treats a bare plus as a space in query parameters.
        components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        guard let url = components.url else { throw LyricsFailure.invalid }
        var request = URLRequest(url: url)
        request.timeoutInterval = timeout
        request.setValue(userAgent, forHTTPHeaderField: "User-Agent")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        do {
            let start = Date()
            let (bytes, response) = try await session.bytes(for: request)
            guard let http = response as? HTTPURLResponse, response.url == url else { throw LyricsFailure.network }
            let elapsedMs = Int(Date().timeIntervalSince(start) * 1000)
            Self.log.debug("LRCLIB \(path, privacy: .public) HTTP \(http.statusCode, privacy: .public) in \(elapsedMs, privacy: .public) ms")
            if http.statusCode == 404 { throw LyricsFailure.missing }
            if http.statusCode == 429 || http.statusCode == 503 {
                let minimum = Self.retryDelay(http.value(forHTTPHeaderField: "Retry-After"))
                let delay = minimum * Double.random(in: 1...1.3)
                retryAfter = Date().addingTimeInterval(delay); throw LyricsFailure.rateLimited(Int(ceil(delay)))
            }
            guard http.statusCode == 200, http.mimeType == "application/json", response.expectedContentLength <= 1024 * 1024 else { throw LyricsFailure.network }
            var data = Data()
            for try await byte in bytes {
                try Task.checkCancellation()
                guard data.count < 1024 * 1024 else { throw LyricsFailure.invalid }
                data.append(byte)
            }
            try Task.checkCancellation()
            return try JSONDecoder().decode(T.self, from: data)
        } catch is CancellationError { throw CancellationError() }
        catch let error as LyricsFailure { throw error }
        catch {
            if Task.isCancelled { throw CancellationError() }
            if let error = error as? URLError {
                Self.log.debug("LRCLIB transport error \(error.code.rawValue, privacy: .public)")
            } else {
                Self.log.debug("LRCLIB rejected a response that did not match its JSON format")
            }
            throw LyricsFailure.network
        }
    }

    private static func lyricsResult(_ reply: Reply, duration: UInt64) -> LyricsResult? {
        let document: LyricsDocument
        if reply.instrumental {
            document = LyricsDocument(lines: [], instrumental: true, precision: "plain")
        } else {
            var best: LyricsDocument?
            for (format, text) in [("lyricsfile", reply.lyricsfile), ("lrc", reply.syncedLyrics), ("plain", reply.plainLyrics)] {
                if format == "plain", best != nil { break }
                guard let text else { continue }
                guard let parsed = try? parse(text, format: format, duration: duration), parsed.hasTranslatableText else {
                    log.debug("LRCLIB parser could not use \(format, privacy: .public) payload")
                    continue
                }
                if precision(of: parsed) > precision(of: best) { best = parsed }
                if parsed.precision == "word" { break }
            }
            guard let best else { return nil }
            document = best
        }
        return LyricsResult(document: document, source: "LRCLIB", sourceURL: URL(string: "https://lrclib.net/api/get/\(reply.id)"), imported: false)
    }
    private static func precision(of document: LyricsDocument?) -> Int {
        switch document?.precision {
        case "word": 3
        case "line": 2
        case "plain": 1
        default: 0
        }
    }
    static func canonical(_ string: String) -> String {
        string.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: Locale(identifier: "en_US_POSIX")).split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }
    static func retryDelay(_ value: String?) -> TimeInterval {
        if let value, let seconds = Double(value), seconds.isFinite, seconds >= 0 { return min(86400, max(1, seconds)) }
        if let value {
            let formatter = DateFormatter(); formatter.locale = Locale(identifier: "en_US_POSIX"); formatter.timeZone = TimeZone(secondsFromGMT: 0)
            formatter.dateFormat = "EEE, dd MMM yyyy HH:mm:ss zzz"
            if let date = formatter.date(from: value) { return min(86400, max(1, date.timeIntervalSinceNow)) }
        }
        return 30
    }
    private func read(_ url: URL, limit: Int) throws -> Data {
        guard (try url.resourceValues(forKeys: [.isRegularFileKey])).isRegularFile == true else { throw LyricsFailure.invalid }
        let file = try FileHandle(forReadingFrom: url); defer { try? file.close() }
        let data = try file.read(upToCount: limit + 1) ?? Data()
        guard !data.isEmpty, data.count <= limit else { throw LyricsFailure.invalid }
        return data
    }
    private static func decodeImport(_ data: Data, format: String) throws -> String {
        let text: String?
        if format == "lrc", data.starts(with: [0xFF, 0xFE]) || data.starts(with: [0xFE, 0xFF]) {
            guard data.count.isMultiple(of: 2) else { throw LyricsFailure.invalid }
            text = String(data: data, encoding: .utf16)
        } else {
            let bytes = data.starts(with: [0xEF, 0xBB, 0xBF]) ? data.dropFirst(3) : data
            text = String(data: bytes, encoding: .utf8)
        }
        guard let text else { throw LyricsFailure.invalid }
        return text
    }
    func importFile(_ url: URL, for selection: LyricsSelection) async throws -> LyricsResult {
        do { try loadCache() } catch { throw LyricsFailure.storage }
        let scoped = url.startAccessingSecurityScopedResource(); defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let data = try read(url, limit: 512 * 1024)
        let type = url.pathExtension.lowercased()
        let format = ["yaml", "yml"].contains(type) ? "lyricsfile" : type
        let text = try Self.decodeImport(data, format: format)
        let parsed = try Self.parse(text, format: format, duration: selection.durationMs)
        let result = LyricsResult(document: parsed, source: "Imported lyrics", sourceURL: nil, imported: true)
        var updated = imports.filter { !$0.selection.sameRecording(as: selection) }
        updated.append(Record(selection: selection, result: result))
        while updated.count > 32 { updated.removeFirst() }
        let encoded = try JSONEncoder().encode(updated)
        guard encoded.count <= 4 * 1024 * 1024 else { throw LyricsFailure.storage }
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
            let file = directory.appendingPathComponent("imports.json")
            try encoded.write(to: file, options: .atomic)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
        } catch { throw LyricsFailure.storage }
        imports = updated; remember(result, for: selection); return result
    }
    static func parse(_ text: String, format: String, duration: UInt64) throws -> LyricsDocument {
        struct Request: Encodable { let format, text: String; let duration_ms: UInt64 }
        let request = try JSONEncoder().encode(Request(format: format, text: text, duration_ms: duration))
        guard request.count <= 1024 * 1024 else { throw LyricsFailure.invalid }
        var length = 0
        let status = request.withUnsafeBytes { raw in player_lyrics_parse(raw.baseAddress?.assumingMemoryBound(to: UInt8.self), request.count, nil, 0, &length) }
        guard status == 2, length > 0, length <= 2 * 1024 * 1024 else { throw LyricsFailure.invalid }
        var output = Data(count: length)
        let copied = output.withUnsafeMutableBytes { buffer in request.withUnsafeBytes { raw in
            player_lyrics_parse(raw.baseAddress?.assumingMemoryBound(to: UInt8.self), request.count, buffer.baseAddress?.assumingMemoryBound(to: UInt8.self), buffer.count, &length)
        } }
        guard copied == 0 else { throw LyricsFailure.invalid }
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(LyricsDocument.self, from: output)
    }
}
