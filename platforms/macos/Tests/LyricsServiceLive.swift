import Foundation

private final class LiveLyricsRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

/// Explicit read-only live gate; metadata supplied on the CLI, never account credentials or lyrics in output.
@main private enum LyricsServiceLive {
    static func main() async {
        guard CommandLine.arguments.count == 5, let duration = UInt64(CommandLine.arguments[4]) else {
            print("Usage: lyrics-service-live title artist album duration_ms"); exit(2)
        }
        let environment = ProcessInfo.processInfo.environment
        let id = environment["AUVRA_LYRICS_TEST_SPOTIFY_ID"].map { "spotify:track:\($0)" } ?? "live-lyrics-check"
        let item = MediaItem(id: id, title: CommandLine.arguments[1], artists: [CommandLine.arguments[2]], album: CommandLine.arguments[3], durationMs: duration, artwork: nil, capabilities: MediaCapabilities(play: true, pause: true, seek: true, volume: true, pcmAnalysis: false, systemMediaSession: false))
        do {
            let directory = FileManager.default.temporaryDirectory.appendingPathComponent("auvra-lyrics-live-" + UUID().uuidString)
            let lrcRedContact = LrcRedContact(projectURL: environment["AUVRA_LYRICS_TEST_PROJECT_URL"], email: environment["AUVRA_LYRICS_TEST_CONTACT_EMAIL"])
            let selection = LyricsSelection(item)
            let result: LyricsResult
            if environment["AUVRA_LYRICS_TEST_MUSIXMATCH_ONLY"] == "1" {
                guard let match = try await MusixmatchLyrics(session: anonymousSession()).lookup(selection) else {
                    print("Musixmatch live gate found no verified lyrics")
                    exit(1)
                }
                result = match
            } else if environment["AUVRA_LYRICS_TEST_LRC_RED_ONLY"] == "1" {
                guard let lrcRedContact else { print("lrc.red live gate requires a project contact"); exit(2) }
                let userAgent = lrcRedContact.userAgent(app: "Auvra/0.1.0 (macOS; app.auvra.player)")
                guard let match = try await LrcRedLyrics(session: anonymousSession(), userAgent: userAgent).lookup(selection) else {
                    print("lrc.red live gate found no verified timed lyrics")
                    exit(1)
                }
                result = match
            } else {
                result = try await LyricsService(directory: directory, lrcRedContact: lrcRedContact,
                                                 enableMusixmatch: environment["AUVRA_LYRICS_TEST_MUSIXMATCH"] == "1").lookup(selection)
            }
            guard result.document.isValid else { print("Live lyrics gate failed validation"); exit(1) }
            print("Live lyrics gate passed: source=\(result.source), precision=\(result.document.precision), instrumental=\(result.document.instrumental), lines=\(result.document.lines.count), words=\(result.document.lines.reduce(0) { $0 + $1.words.count }), credentials=none")
        } catch { print("Live lyrics gate: \((error as? LyricsFailure)?.errorDescription ?? "request failed")"); exit(1) }
    }

    private static func anonymousSession() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.urlCredentialStorage = nil
        return URLSession(configuration: configuration, delegate: LiveLyricsRedirects(), delegateQueue: nil)
    }
}
