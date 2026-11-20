import AppKit
import CryptoKit
import Foundation
import Security

enum SpotifyLoginFailure: LocalizedError {
    case invalidClient, cancelled, invalidCallback, timedOut, authorization, invalidGrant, network, permissions, callbackUnavailable
    case temporarilyUnavailable(retryAfter: TimeInterval)
    var refreshRetry: (delay: TimeInterval, message: String)? {
        switch self {
        case .network: (0, "Auvra could not reach Spotify and will retry automatically.")
        case .temporarilyUnavailable(let delay): (delay, "Spotify is temporarily unavailable. Auvra will retry automatically.")
        default: nil
        }
    }
    var errorDescription: String? {
        switch self {
        case .invalidClient: "Enter the 32-character Client ID from your Spotify app."
        case .cancelled: "Spotify login was cancelled."
        case .invalidCallback: "The Spotify login response could not be verified."
        case .timedOut: "Spotify login timed out. Try connecting again."
        case .authorization: "Spotify denied this login. Check the app settings and allowed users."
        case .invalidGrant: "The saved Spotify login expired or was revoked. Connect again."
        case .network: "Spotify login could not be reached. Check your connection and try again."
        case .temporarilyUnavailable: "Spotify login is temporarily unavailable. Try again shortly."
        case .permissions: "Spotify did not grant the permissions needed for playback and your library."
        case .callbackUnavailable: "Auvra could not open the local Spotify login port. Finish any other Auvra login attempt and try again."
        }
    }
}
struct SpotifyPKCE: Sendable {
    let verifier, state: String
    init() throws { verifier = try Self.random(); state = try Self.random() }
    init(verifier: String, state: String) { self.verifier = verifier; self.state = state }
    var challenge: String { Self.base64(Data(SHA256.hash(data: Data(verifier.utf8)))) }
    static func base64(_ data: Data) -> String { data.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "") }
    private static func random() throws -> String {
        var bytes = [UInt8](repeating: 0, count: 64)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else { throw SpotifyLoginFailure.authorization }
        return base64(Data(bytes))
    }
    func authorizationURL(clientID: String, redirect: URL, showDialog: Bool = false) -> URL {
        var url = URLComponents(string: "https://accounts.spotify.com/authorize")!
        url.queryItems = [URLQueryItem(name: "client_id", value: clientID), .init(name: "response_type", value: "code"), .init(name: "redirect_uri", value: redirect.absoluteString), .init(name: "state", value: state), .init(name: "scope", value: SpotifyOAuthClient.scopes.joined(separator: " ")), .init(name: "code_challenge_method", value: "S256"), .init(name: "code_challenge", value: challenge)]
        if showDialog { url.queryItems?.append(.init(name: "show_dialog", value: "true")) }
        return url.url!
    }
    static func callbackCode(_ target: String, expectedState: String) throws -> String {
        guard target.utf8.count <= 8192, let url = URLComponents(string: "http://127.0.0.1" + target), url.path == "/spotify/callback" else { throw SpotifyLoginFailure.invalidCallback }
        let query = url.queryItems ?? []
        guard query.filter({ $0.name == "state" }).count == 1, query.first(where: { $0.name == "state" })?.value == expectedState else { throw SpotifyLoginFailure.invalidCallback }
        if query.contains(where: { $0.name == "error" }) { throw SpotifyLoginFailure.authorization }
        guard query.filter({ $0.name == "code" }).count == 1, let code = query.first(where: { $0.name == "code" })?.value, !code.isEmpty, code.utf8.count <= 4096 else { throw SpotifyLoginFailure.invalidCallback }
        return code
    }
}
private final class NoOAuthRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}
struct SpotifyTokenReply: Decodable, Sendable {
    let accessToken: String
    let refreshToken: String?
    let expiresIn: Int
    let tokenType: String
    let scope: String?
    var spokenProgressGranted: Bool? {
        scope.map { $0.split(separator: " ").contains { $0 == SpotifyOAuthClient.spokenProgressScope } }
    }
}
actor SpotifyOAuthClient {
    static let callbackPort: UInt16 = 43821
    static let redirectURI = "http://127.0.0.1:43821/spotify/callback"
    static let spokenProgressScope = "user-read-playback-position"
    static let requiredScopes = ["user-read-playback-state", "user-modify-playback-state", "user-read-currently-playing", "user-library-read", "playlist-read-private", "playlist-read-collaborative"]
    static let scopes = requiredScopes + [
        "user-library-modify", "user-follow-read", "user-follow-modify", "user-top-read", "user-read-recently-played",
        spokenProgressScope, "playlist-modify-public", "playlist-modify-private", "ugc-image-upload"
    ]
    private let session: URLSession
    init(session: URLSession? = nil) {
        let config = URLSessionConfiguration.ephemeral; config.urlCache = nil; config.httpCookieStorage = nil
        self.session = session ?? URLSession(configuration: config, delegate: NoOAuthRedirects(), delegateQueue: nil)
    }
    func exchange(clientID: String, code: String, verifier: String, redirect: URL) async throws -> SpotifyTokenReply {
        try await token(["grant_type": "authorization_code", "client_id": clientID, "code": code, "code_verifier": verifier, "redirect_uri": redirect.absoluteString])
    }
    func refresh(clientID: String, refreshToken: String) async throws -> SpotifyTokenReply {
        try await token(["grant_type": "refresh_token", "client_id": clientID, "refresh_token": refreshToken])
    }
    private func token(_ values: [String: String]) async throws -> SpotifyTokenReply {
        var encoded = URLComponents(); encoded.queryItems = values.sorted(by: { $0.key < $1.key }).map { URLQueryItem(name: $0.key, value: $0.value) }
        var request = URLRequest(url: URL(string: "https://accounts.spotify.com/api/token")!)
        request.httpMethod = "POST"; request.timeoutInterval = 15
        request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        request.httpBody = Data((encoded.percentEncodedQuery ?? "").replacingOccurrences(of: "+", with: "%2B").utf8)
        let data: Data; let response: URLResponse
        do { (data, response) = try await session.data(for: request) } catch { throw SpotifyLoginFailure.network }
        guard data.count <= 64 * 1024, let response = response as? HTTPURLResponse else { throw SpotifyLoginFailure.network }
        if response.statusCode == 400, (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"] as? String == "invalid_grant" { throw SpotifyLoginFailure.invalidGrant }
        if response.statusCode == 429 || (500...599).contains(response.statusCode) {
            let seconds = response.value(forHTTPHeaderField: "Retry-After").flatMap { Double($0.trimmingCharacters(in: .whitespacesAndNewlines)) }
            let delay = seconds.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil } ?? (response.statusCode == 429 ? 30 : 0)
            throw SpotifyLoginFailure.temporarilyUnavailable(retryAfter: delay)
        }
        guard response.statusCode == 200 else { throw SpotifyLoginFailure.authorization }
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let token = try? decoder.decode(SpotifyTokenReply.self, from: data) else { throw SpotifyLoginFailure.authorization }
        guard token.tokenType.lowercased() == "bearer", !token.accessToken.isEmpty, token.accessToken.utf8.count <= 8192, token.expiresIn > 0, token.expiresIn <= 86400 else { throw SpotifyLoginFailure.authorization }
        if let scope = token.scope, !Set(Self.requiredScopes).isSubset(of: Set(scope.split(separator: " ").map(String.init))) { throw SpotifyLoginFailure.permissions }
        return token
    }
}
