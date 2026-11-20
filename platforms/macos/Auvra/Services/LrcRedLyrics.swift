import Foundation

/// Optional timed lyrics source. The host supplies the configured app contact.
actor LrcRedLyrics {
    static let source = "lrc.red"
    private static let baseURL = URL(string: "https://lrc.red")!
    private static let timeout: TimeInterval = 2
    private let session: URLSession
    private let userAgent: String
    private var retryAfter = Date.distantPast

    private struct Track: Decodable {
        let isrc, title, artist: String
        let album: String?
        let duration: Double?

        var durationMs: UInt64? {
            guard let duration, duration.isFinite, (1...3600).contains(duration) else { return nil }
            return UInt64((duration * 1000).rounded())
        }

        var hasValidISRC: Bool {
            let bytes = isrc.utf8
            return bytes.count == 12 && bytes.prefix(2).allSatisfy { (65...90).contains($0) }
                && bytes.dropFirst(2).prefix(3).allSatisfy { (65...90).contains($0) || (48...57).contains($0) }
                && bytes.suffix(7).allSatisfy { (48...57).contains($0) }
        }

        func matchesIdentity(_ selection: LyricsSelection) -> Bool {
            hasValidISRC && LyricsService.canonical(title) == LyricsService.canonical(selection.title)
                && LyricsService.canonical(artist) == LyricsService.canonical(selection.artist)
        }

        func durationDifference(_ selection: LyricsSelection) -> Double {
            guard let durationMs else { return .infinity }
            return abs(Double(durationMs) - Double(selection.durationMs))
        }
    }

    private struct SearchResponse: Decodable {
        struct Hit: Decodable {
            let track: Track?
            init(from decoder: any Decoder) { track = try? Track(from: decoder) }
        }
        let hits: [Hit]
    }

    init(session: URLSession, userAgent: String) {
        self.session = session
        self.userAgent = userAgent
    }

    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult? {
        try Task.checkCancellation()
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
        // Punctuation and the guest credit can hide an otherwise exact search hit.
        // The result still has to match the complete recording title.
        let baseTitle = selection.title.replacingOccurrences(
            of: #"\s*[\[(](?:feat\.?|ft\.?|featuring)\s+.+[)\]]\s*$"#,
            with: "", options: [.caseInsensitive, .regularExpression]
        )
        let title = baseTitle.isEmpty ? selection.title : baseTitle
        let query = [URLQueryItem(name: "q", value: "\(title) \(selection.artist)")]
        guard let data = try await fetch("/search.json", query: query, mimeTypes: ["application/json"]),
              let search = try? JSONDecoder().decode(SearchResponse.self, from: data),
              let track = try await verifiedTrack(in: search, selection: selection), let durationMs = track.durationMs else {
            return nil
        }
        let path = "/s/\(track.isrc)"
        let parsingDuration = max(durationMs, selection.durationMs)
        for (suffix, format, mimeTypes) in [
            (".ttml", "ttml-amll", ["application/ttml+xml", "application/xml", "text/xml", "text/plain"]),
            (".lrc", "lrc", ["text/plain"])
        ] {
            guard let data = try await fetch(path + suffix, mimeTypes: mimeTypes),
                  let text = String(data: data, encoding: .utf8),
                  let document = try? LyricsService.parse(text, format: format, duration: parsingDuration),
                  document.hasTiming, document.hasTranslatableText else { continue }
            if format == "ttml-amll", document.precision == "line", !ExplicitParagraphTimes.validate(text) { continue }
            let voices = document.lines.flatMap(\.voicesWithText).filter { $0.startMs != nil }
            guard !voices.isEmpty, voices.allSatisfy({
                ($0.startMs ?? selection.durationMs) < selection.durationMs && ($0.endMs ?? parsingDuration) <= parsingDuration
            }) else { continue }
            try Task.checkCancellation()
            return LyricsResult(document: document, source: Self.source,
                                sourceURL: URL(string: path, relativeTo: Self.baseURL)?.absoluteURL, imported: false)
        }
        return nil
    }

    private func verifiedTrack(in search: SearchResponse, selection: LyricsSelection) async throws -> Track? {
        let album = LyricsService.canonical(selection.album)
        let matches = search.hits.compactMap(\.track).filter { $0.matchesIdentity(selection) }.sorted {
            let firstOnAlbum = !album.isEmpty && LyricsService.canonical($0.album ?? "") == album
            let secondOnAlbum = !album.isEmpty && LyricsService.canonical($1.album ?? "") == album
            if firstOnAlbum != secondOnAlbum { return firstOnAlbum }
            return $0.durationDifference(selection) < $1.durationDifference(selection)
        }
        if let known = matches.first(where: { $0.durationDifference(selection) <= 2100 }) { return known }
        guard let incomplete = matches.first(where: { $0.duration == nil }),
              let data = try await fetch("/s/\(incomplete.isrc).json", mimeTypes: ["application/json"]),
              let detail = try? JSONDecoder().decode(Track.self, from: data),
              detail.isrc == incomplete.isrc, detail.matchesIdentity(selection),
              detail.durationDifference(selection) <= 2100 else { return nil }
        return detail
    }

    private func fetch(_ path: String, query: [URLQueryItem] = [], mimeTypes: [String]) async throws -> Data? {
        try Task.checkCancellation()
        guard Date() >= retryAfter else { return nil }
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
        request.setValue(mimeTypes.joined(separator: ", "), forHTTPHeaderField: "Accept")
        do {
            let (bytes, response) = try await session.bytes(for: request)
            guard let http = response as? HTTPURLResponse, response.url == url else { return nil }
            if http.statusCode == 429 || http.statusCode == 503 {
                let delay = LyricsService.retryDelay(http.value(forHTTPHeaderField: "Retry-After"))
                retryAfter = Date().addingTimeInterval(delay + Double.random(in: 0...(delay * 0.3)))
                return nil
            }
            let limit = 512 * 1024
            guard http.statusCode == 200, mimeTypes.contains(http.mimeType ?? ""), http.expectedContentLength <= limit else { return nil }
            var body = Data()
            for try await byte in bytes {
                try Task.checkCancellation()
                guard body.count < limit else { return nil }
                body.append(byte)
            }
            return body
        } catch {
            try Task.checkCancellation()
            return nil
        }
    }
}
