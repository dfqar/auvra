import Foundation
import OSLog

/// Experimental client for anonymous Musixmatch mobile sessions.
actor MusixmatchLyrics {
    static let source = "Musixmatch"
    private static let host = "apic-appmobile.musixmatch.com"
    private static let appID = "mac-ios-v2.0"
    private static let timeout: TimeInterval = 3
    private static let log = Logger(subsystem: "app.auvra.player", category: "Lyrics")

    private enum Operation: String {
        case token = "token.get"
        case lyrics = "macro.subtitles.get"
    }

    private final class RejectRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
        func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                        newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
            completionHandler(nil)
        }
    }

    private struct Token {
        let value: String
        let expiresAt: Date
    }

    private struct Reply<Body: Decodable>: Decodable {
        let status: Int
        let body: Body?

        private enum Key: String, CodingKey { case message, header, body }
        private struct Header: Decodable { let status_code: Int }

        init(from decoder: any Decoder) throws {
            let outer = try decoder.container(keyedBy: Key.self)
            let content = outer.contains(.message) ? try outer.nestedContainer(keyedBy: Key.self, forKey: .message) : outer
            status = try content.decode(Header.self, forKey: .header).status_code
            body = try? content.decode(Body.self, forKey: .body)
        }
    }

    /// The service uses numeric flags and some client versions use JSON booleans.
    private struct Flag: Decodable {
        let active: Bool

        init(from decoder: any Decoder) throws {
            let value = try decoder.singleValueContainer()
            if let numeric = try? value.decode(Int.self) { active = numeric != 0 }
            else { active = (try? value.decode(Bool.self)) ?? true }
        }
    }

    private struct TokenBody: Decodable { let user_token: String }
    private struct MacroBody: Decodable { let macro_calls: Calls }
    private struct Match: Decodable { let track: Track }
    private struct Track: Decodable {
        let track_id: Int64
        let track_name: String
        let artist_name: String
        let track_length: Double
        let track_spotify_id: String?
        let restricted: Flag?
        let instrumental: Flag?
    }
    private struct LyricsContent: Decodable {
        let richsync_body, subtitle_body, lyrics_body: String?
        let restricted: Flag?

        private enum Key: String, CodingKey { case richsync_body, subtitle_body, lyrics_body, restricted }

        init(from decoder: any Decoder) throws {
            let content = try decoder.container(keyedBy: Key.self)
            // Keep the restriction flag even when this particular format is unusable.
            restricted = try content.decodeIfPresent(Flag.self, forKey: .restricted)
            richsync_body = try? content.decode(String.self, forKey: .richsync_body)
            subtitle_body = try? content.decode(String.self, forKey: .subtitle_body)
            lyrics_body = try? content.decode(String.self, forKey: .lyrics_body)
        }
    }
    private struct Richsync: Decodable { let richsync: LyricsContent }
    private struct Subtitles: Decodable {
        struct Entry: Decodable { let subtitle: LyricsContent }
        let subtitle_list: [Entry]
    }
    private struct Lyrics: Decodable { let lyrics: LyricsContent }
    private struct Calls: Decodable {
        let matcher: Reply<Match>
        let richsync: Reply<Richsync>?
        let subtitles: Reply<Subtitles>?
        let lyrics: Reply<Lyrics>?

        private enum CodingKeys: String, CodingKey {
            case matcher = "matcher.track.get"
            case richsync = "track.richsync.get"
            case subtitles = "track.subtitles.get"
            case lyrics = "track.lyrics.get"
        }

        var statuses: [Int] {
            [matcher.status, richsync?.status, subtitles?.status, lyrics?.status].compactMap { $0 }
        }

        var isRestricted: Bool {
            statuses.contains(403) || statuses.contains(451)
                || matcher.body?.track.restricted?.active == true
                || richsync?.body?.richsync.restricted?.active == true
                || lyrics?.body?.lyrics.restricted?.active == true
                || subtitles?.body?.subtitle_list.contains { $0.subtitle.restricted?.active == true } == true
        }
    }

    private let session: URLSession
    private let redirects = RejectRedirects()
    private var token: Token?
    private var retryAfter = Date.distantPast
    private var isLookingUp = false

    init(session: URLSession) { self.session = session }

    func lookup(_ selection: LyricsSelection) async throws -> LyricsResult? {
        try Task.checkCancellation()
        guard !isLookingUp, Date() >= retryAfter, !selection.title.isEmpty, !selection.artist.isEmpty,
              (1000...3_600_000).contains(selection.durationMs) else { return nil }
        isLookingUp = true
        defer { isLookingUp = false }
        return try await withThrowingTaskGroup(of: LyricsResult?.self) { group in
            group.addTask { try await self.fetchLyrics(selection) }
            group.addTask {
                try await Task.sleep(for: .seconds(Self.timeout))
                return nil
            }
            defer { group.cancelAll() }
            let result = try await group.next() ?? nil
            try Task.checkCancellation()
            return result
        }
    }

    private func fetchLyrics(_ selection: LyricsSelection) async throws -> LyricsResult? {
        let spotifyID = selection.id.hasPrefix("spotify:track:") ? Self.spotifyID(selection.id) : nil
        if selection.id.hasPrefix("spotify:track:"), spotifyID == nil { return nil }
        guard let token = try await anonymousToken() else { return nil }
        var query = [
            URLQueryItem(name: "format", value: "json"),
            URLQueryItem(name: "namespace", value: "lyrics_richsynched"),
            URLQueryItem(name: "subtitle_format", value: "mxm"),
            URLQueryItem(name: "q_track", value: selection.title),
            URLQueryItem(name: "q_artist", value: selection.artist),
            URLQueryItem(name: "q_artists", value: selection.artist),
            URLQueryItem(name: "q_album", value: selection.album),
            URLQueryItem(name: "q_duration", value: String(Double(selection.durationMs) / 1000)),
            URLQueryItem(name: "f_subtitle_length", value: String(selection.durationMs / 1000)),
            URLQueryItem(name: "usertoken", value: token),
            URLQueryItem(name: "optional_calls", value: "track.richsync"),
            URLQueryItem(name: "richsync_compact_type", value: "words")
        ]
        if let spotifyID { query.append(URLQueryItem(name: "track_spotify_id", value: "spotify:track:\(spotifyID)")) }
        guard let reply: MacroBody = try await fetch(.lyrics, query: query) else { return nil }
        let calls = reply.macro_calls
        if calls.matcher.status == 401 {
            _ = accept(401, retryHeader: nil)
        } else if let rateStatus = calls.statuses.first(where: { $0 == 429 || $0 == 503 }) {
            _ = accept(rateStatus, retryHeader: nil)
        }
        guard !calls.isRestricted, calls.matcher.status == 200, let track = calls.matcher.body?.track,
              Self.matches(track, selection: selection, spotifyID: spotifyID) else { return nil }
        let richsync = calls.richsync?.status == 200 ? calls.richsync?.body?.richsync.richsync_body : nil
        let subtitle = calls.subtitles?.status == 200 ? calls.subtitles?.body?.subtitle_list.first?.subtitle.subtitle_body : nil
        let plain = calls.lyrics?.status == 200 ? calls.lyrics?.body?.lyrics.lyrics_body : nil
        guard let document = MusixmatchPayload.document(richsync: richsync, subtitle: subtitle, plain: plain,
                                                       durationMs: selection.durationMs) else { return nil }
        try Task.checkCancellation()
        return LyricsResult(document: document, source: Self.source, sourceURL: URL(string: "https://www.musixmatch.com"), imported: false)
    }

    private static func matches(_ track: Track, selection: LyricsSelection, spotifyID: String?) -> Bool {
        guard track.track_id > 0, track.instrumental?.active != true, track.track_length.isFinite,
              abs(track.track_length * 1000 - Double(selection.durationMs)) <= 2100,
              LyricsService.canonical(track.track_name) == LyricsService.canonical(selection.title) else { return false }
        if let spotifyID, let returned = track.track_spotify_id, !returned.isEmpty, Self.spotifyID(returned) != spotifyID { return false }
        let returnedArtist = LyricsService.canonical(track.artist_name)
        if returnedArtist == LyricsService.canonical(selection.artist) { return true }
        let credits = [selection.artist] + (selection.additionalArtists ?? [])
        return credits.count > 1 && [", ", " & ", " and ", " x ", " feat. ", " featuring "].contains {
            returnedArtist == LyricsService.canonical(credits.joined(separator: $0))
        }
    }

    private static func spotifyID(_ value: String) -> String? {
        let id = value.hasPrefix("spotify:track:") ? String(value.dropFirst("spotify:track:".count)) : value
        guard id.utf8.count == 22, id.utf8.allSatisfy({
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0)
        }) else { return nil }
        return id
    }

    private func anonymousToken() async throws -> String? {
        if let token, token.expiresAt > Date() { return token.value }
        token = nil
        guard let response: TokenBody = try await fetch(.token), (1...512).contains(response.user_token.utf8.count),
              response.user_token.utf8.allSatisfy({ (33...126).contains($0) }) else {
            try Task.checkCancellation()
            retryAfter = max(retryAfter, Date().addingTimeInterval(30))
            return nil
        }
        try Task.checkCancellation()
        token = Token(value: response.user_token, expiresAt: Date().addingTimeInterval(600))
        return response.user_token
    }

    private func fetch<Body: Decodable>(_ operation: Operation, query: [URLQueryItem] = []) async throws -> Body? {
        try Task.checkCancellation()
        guard Date() >= retryAfter else { return nil }
        var components = URLComponents()
        components.scheme = "https"
        components.host = Self.host
        components.path = "/ws/1.1/\(operation.rawValue)"
        components.queryItems = [URLQueryItem(name: "app_id", value: Self.appID)] + query
        components.percentEncodedQuery = components.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        guard let url = components.url else { return nil }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: Self.timeout)
        request.httpShouldHandleCookies = false
        request.setValue("Musixmatch/2025120901 CFNetwork/3860.300.31 Darwin/25.2.0", forHTTPHeaderField: "X-User-Agent")
        request.setValue("10.1.1", forHTTPHeaderField: "x-mxm-app-version")
        request.setValue("x-mxm-token-guid=", forHTTPHeaderField: "X-Cookie")
        request.setValue("en-US,en;q=0.9", forHTTPHeaderField: "Accept-Language")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        do {
            let (bytes, response) = try await session.bytes(for: request, delegate: redirects)
            guard let http = response as? HTTPURLResponse, response.url == url else { return nil }
            Self.log.debug("Musixmatch \(operation.rawValue, privacy: .public) HTTP \(http.statusCode, privacy: .public)")
            guard accept(http.statusCode, retryHeader: http.value(forHTTPHeaderField: "Retry-After")) else { return nil }
            let limit = 1024 * 1024
            guard ["application/json", "text/plain"].contains(http.mimeType ?? ""), http.expectedContentLength <= limit else { return nil }
            var data = Data()
            for try await byte in bytes {
                try Task.checkCancellation()
                guard data.count < limit else { return nil }
                data.append(byte)
            }
            let reply = try JSONDecoder().decode(Reply<Body>.self, from: data)
            Self.log.debug("Musixmatch \(operation.rawValue, privacy: .public) API \(reply.status, privacy: .public)")
            guard accept(reply.status, retryHeader: http.value(forHTTPHeaderField: "Retry-After")) else { return nil }
            return reply.body
        } catch {
            try Task.checkCancellation()
            Self.log.debug("Musixmatch \(operation.rawValue, privacy: .public) response unavailable")
            return nil
        }
    }

    private func accept(_ status: Int, retryHeader: String?) -> Bool {
        switch status {
        case 200: return true
        case 401, 403, 451:
            token = nil
            retryAfter = Date().addingTimeInterval(max(60, LyricsService.retryDelay(retryHeader)))
        case 429, 503:
            let delay = LyricsService.retryDelay(retryHeader)
            retryAfter = Date().addingTimeInterval(delay * Double.random(in: 1...1.3))
        default: break
        }
        return false
    }
}
