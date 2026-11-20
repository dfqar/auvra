import Foundation

/// Community timings, with recording metadata verified before fetching the selected entry.
actor UnisonLyrics {
    static let source = "Unison"
    private static let baseURL = URL(string: "https://unison.boidu.dev")!
    private static let timeout: TimeInterval = 2
    private let session: URLSession
    private let userAgent: String
    private var retryAfter = Date.distantPast

    private struct Response<Value: Decodable>: Decodable {
        let success: Bool
        let data: Value?
    }

    private struct Entry: Decodable {
        let id: Int64
        let videoId, song, artist, format: String
        let album, lyrics: String?
        let duration: Double?
        let hidden: Bool?

        func matches(_ selection: LyricsSelection, artists: Set<String>) -> Bool {
            id > 0 && hidden != true && videoId.utf8.count == 11 && videoId.utf8.allSatisfy {
                (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95
            } && LyricsService.canonical(song) == LyricsService.canonical(selection.title)
                && artists.contains(LyricsService.canonical(artist)) && ["ttml", "lrc"].contains(format)
        }

        func durationDifference(_ selection: LyricsSelection) -> Double {
            guard let duration, duration.isFinite else { return .infinity }
            return abs(duration * 1000 - Double(selection.durationMs))
        }
    }

    private struct SearchHit: Decodable {
        let entry: Entry?
        init(from decoder: any Decoder) { entry = try? Entry(from: decoder) }
    }

    init(session: URLSession, userAgent: String) {
        self.session = session
        self.userAgent = userAgent
    }

    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult? {
        guard Date() >= retryAfter else { return nil }
        return try await withThrowingTaskGroup(of: LyricsResult?.self) { group in
            group.addTask { try await self.fetchLyrics(selection) }
            group.addTask {
                try await Task.sleep(for: .seconds(Self.timeout))
                return nil
            }
            defer { group.cancelAll() }
            return try await group.next() ?? nil
        }
    }

    private func fetchLyrics(_ selection: LyricsSelection) async throws -> LyricsResult? {
        let query = [
            URLQueryItem(name: "q", value: "\(selection.title) \(selection.artist)"),
            URLQueryItem(name: "limit", value: "5")
        ]
        // The search response includes recording duration, which the detail response may omit.
        guard let hits: [SearchHit] = try await fetch("/lyrics/search", query: query) else { return nil }
        let artists = Set(([selection.artist] + (selection.additionalArtists ?? [])).map(LyricsService.canonical))
        let album = LyricsService.canonical(selection.album)
        let candidates = hits.compactMap(\.entry).filter { $0.matches(selection, artists: artists) && $0.durationDifference(selection) <= 2100 }
        let match = candidates.min {
            let firstOnAlbum = !album.isEmpty && LyricsService.canonical($0.album ?? "") == album
            let secondOnAlbum = !album.isEmpty && LyricsService.canonical($1.album ?? "") == album
            if firstOnAlbum != secondOnAlbum { return firstOnAlbum }
            return $0.durationDifference(selection) < $1.durationDifference(selection)
        }
        guard let entry = match,
              let detail: Entry = try await fetch("/lyrics/\(entry.id)"),
              detail.id == entry.id, detail.videoId == entry.videoId, detail.format == entry.format,
              detail.matches(selection, artists: artists), detail.duration == nil || detail.durationDifference(selection) <= 2100,
              let lyrics = detail.lyrics, let document = Self.parse(lyrics, format: detail.format, duration: selection.durationMs) else {
            return nil
        }
        try Task.checkCancellation()
        return LyricsResult(document: document, source: Self.source, sourceURL: Self.baseURL, imported: false)
    }

    private func fetch<Value: Decodable>(_ path: String, query: [URLQueryItem] = []) async throws -> Value? {
        try Task.checkCancellation()
        var components = URLComponents(url: Self.baseURL, resolvingAgainstBaseURL: false)!
        components.path = path
        if !query.isEmpty {
            components.queryItems = query
            components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        }
        guard let url = components.url else { return nil }
        var request = URLRequest(url: url)
        request.timeoutInterval = Self.timeout
        request.httpShouldHandleCookies = false
        request.setValue(userAgent, forHTTPHeaderField: "User-Agent")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        do {
            let (bytes, response) = try await session.bytes(for: request)
            guard let http = response as? HTTPURLResponse, response.url == url else { return nil }
            if http.statusCode == 429 || http.statusCode == 503 {
                let delay = LyricsService.retryDelay(http.value(forHTTPHeaderField: "Retry-After"))
                retryAfter = Date().addingTimeInterval(delay + Double.random(in: 0...(delay * 0.3)))
                return nil
            }
            let limit = 1024 * 1024
            guard http.statusCode == 200, http.mimeType == "application/json", http.expectedContentLength <= limit else { return nil }
            var body = Data()
            for try await byte in bytes {
                try Task.checkCancellation()
                guard body.count < limit else { return nil }
                body.append(byte)
            }
            let reply = try JSONDecoder().decode(Response<Value>.self, from: body)
            return reply.success ? reply.data : nil
        } catch {
            try Task.checkCancellation()
            return nil
        }
    }

    private static func parse(_ text: String, format: String, duration: UInt64) -> LyricsDocument? {
        // Composer exports song-absolute spans, as does the Apple profile. Ordinary TTML is parent-relative.
        let usesAbsoluteTime = format == "ttml" && (
            text.contains("https://composer.betterlyrics.org/ttml") || text.contains("http://music.apple.com/lyric-ttml-internal")
        )
        let parserFormat = usesAbsoluteTime ? "ttml-amll" : format
        guard let document = try? LyricsService.parse(text, format: parserFormat, duration: duration),
              document.hasTiming, document.hasTranslatableText else { return nil }
        if format == "ttml", document.precision == "line", !ExplicitParagraphTimes.validate(text) { return nil }
        let timedVoices = document.lines.flatMap(\.voicesWithText).filter { $0.startMs != nil }
        guard !timedVoices.isEmpty, timedVoices.allSatisfy({
            ($0.startMs ?? duration) < duration && ($0.endMs ?? duration) <= duration
        }) else { return nil }
        return document
    }
}

/// The generic TTML parser allows inherited starts. Line-only catalog results must carry explicit cues.
final class ExplicitParagraphTimes: NSObject, XMLParserDelegate {
    private var hasCue = false
    private var missingCue = false

    func parser(_ parser: XMLParser, didStartElement elementName: String, namespaceURI: String?,
                qualifiedName qName: String?, attributes: [String: String]) {
        guard elementName == "p" else { return }
        hasCue = true
        if attributes["begin"] == nil { missingCue = true }
    }

    static func validate(_ text: String) -> Bool {
        let delegate = ExplicitParagraphTimes()
        let parser = XMLParser(data: Data(text.utf8))
        parser.shouldProcessNamespaces = true
        parser.shouldResolveExternalEntities = false
        parser.delegate = delegate
        return parser.parse() && delegate.hasCue && !delegate.missingCue
    }
}
