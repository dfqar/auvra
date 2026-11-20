import Foundation
import Network

/// Mutable listener state has one serial owner, including all continuation resumes.
final class LoopbackOAuthCallback: @unchecked Sendable {
    private let queue = DispatchQueue(label: "app.auvra.oauth-callback")
    private let expectedState: String
    private let requestedPort: UInt16?
    private let timeout: TimeInterval
    private var listener: NWListener?
    private var connections: [ObjectIdentifier: NWConnection] = [:]
    private var startContinuation: CheckedContinuation<URL, Error>?
    private var codeContinuation: CheckedContinuation<String, Error>?
    private var result: Result<String, Error>?
    private var port: UInt16?
    init(state: String, port: UInt16? = nil, timeout: TimeInterval = 600) { expectedState = state; requestedPort = port; self.timeout = timeout }
    func start() async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            queue.async { [self] in
                guard result == nil, listener == nil else { continuation.resume(throwing: SpotifyLoginFailure.cancelled); return }
                startContinuation = continuation
                do {
                    let parameters = NWParameters.tcp
                    let localPort = requestedPort.flatMap { NWEndpoint.Port(String($0)) } ?? .any
                    parameters.requiredLocalEndpoint = .hostPort(host: .ipv4(IPv4Address("127.0.0.1")!), port: localPort)
                    let listener = try NWListener(using: parameters)
                    self.listener = listener
                    listener.stateUpdateHandler = { [weak self, weak listener] state in
                        guard let self else { return }
                        switch state {
                        case .ready:
                            guard let port = listener?.port?.rawValue else { self.finish(.failure(SpotifyLoginFailure.network)); return }
                            self.port = port
                            self.startContinuation?.resume(returning: URL(string: "http://127.0.0.1:\(port)/spotify/callback")!)
                            self.startContinuation = nil
                        case .failed: self.finish(.failure(SpotifyLoginFailure.callbackUnavailable))
                        default: break
                        }
                    }
                    listener.newConnectionHandler = { [weak self] connection in self?.accept(connection) }
                    listener.start(queue: queue)
                    queue.asyncAfter(deadline: .now() + timeout) { [weak self] in self?.finish(.failure(SpotifyLoginFailure.timedOut)) }
                } catch { finish(.failure(SpotifyLoginFailure.callbackUnavailable)) }
            }
        }
    }
    func code() async throws -> String {
        try await withCheckedThrowingContinuation { continuation in
            queue.async { [self] in
                if let result { continuation.resume(with: result) }
                else if codeContinuation == nil { codeContinuation = continuation }
                else { continuation.resume(throwing: SpotifyLoginFailure.cancelled) }
            }
        }
    }
    func cancel() { queue.async { [self] in finish(.failure(SpotifyLoginFailure.cancelled)) } }
    private func accept(_ connection: NWConnection) {
        guard result == nil, connections.count < 4 else { connection.cancel(); return }
        connections[ObjectIdentifier(connection)] = connection
        connection.start(queue: queue)
        receive(connection, bytes: Data())
        queue.asyncAfter(deadline: .now() + 5) { [weak self, weak connection] in
            guard let self, let connection else { return }
            self.connections.removeValue(forKey: ObjectIdentifier(connection)); connection.cancel()
        }
    }
    private func receive(_ connection: NWConnection, bytes: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 8193 - bytes.count) { [weak self] data, _, complete, error in
            guard let self else { connection.cancel(); return }
            var bytes = bytes; if let data { bytes.append(data) }
            guard error == nil, bytes.count <= 8192 else { self.reply(connection, success: false); return }
            if let request = String(data: bytes, encoding: .utf8), request.contains("\r\n\r\n") {
                self.handle(connection, request: request)
            } else if complete { self.reply(connection, success: false) }
            else { self.receive(connection, bytes: bytes) }
        }
    }
    private func handle(_ connection: NWConnection, request: String) {
        let lines = request.components(separatedBy: "\r\n")
        let first = (lines.first ?? "").split(separator: " ")
        let host = lines.dropFirst().first { $0.lowercased().hasPrefix("host:") }?.dropFirst(5).trimmingCharacters(in: .whitespaces)
        guard first.count == 3, first[0] == "GET", first[2] == "HTTP/1.1", let port, host == "127.0.0.1:\(port)" else { reply(connection, success: false); return }
        do {
            let code = try SpotifyPKCE.callbackCode(String(first[1]), expectedState: expectedState)
            reply(connection, success: true); finish(.success(code))
        } catch SpotifyLoginFailure.authorization {
            reply(connection, success: false); finish(.failure(SpotifyLoginFailure.authorization))
        } catch { reply(connection, success: false) }
    }
    private func reply(_ connection: NWConnection, success: Bool) {
        connections.removeValue(forKey: ObjectIdentifier(connection))
        let body = success ? "Spotify login received. You can close this tab and return to Auvra." : "This login response could not be accepted. Return to Auvra and try again."
        let response = "HTTP/1.1 \(success ? "200 OK" : "400 Bad Request")\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: \(body.utf8.count)\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n\(body)"
        connection.send(content: Data(response.utf8), completion: .contentProcessed { _ in connection.cancel() })
    }
    private func finish(_ value: Result<String, Error>) {
        guard result == nil else { return }
        result = value; listener?.cancel(); listener = nil
        if let startContinuation {
            if case .failure(let error) = value { startContinuation.resume(throwing: error) }
            else { startContinuation.resume(throwing: SpotifyLoginFailure.cancelled) }
            self.startContinuation = nil
        }
        codeContinuation?.resume(with: value); codeContinuation = nil
        for connection in connections.values { connection.cancel() }; connections.removeAll()
    }
}
