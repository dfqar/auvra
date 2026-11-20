import AppKit
import Foundation
import Security

typealias SpotifyAuthorizationFactory = @MainActor (
    @escaping (SpotifyAccountStatus) -> Void,
    @escaping (String) -> Void,
    @escaping () -> Void
) -> SpotifyAuthorization

struct SpotifyAccountStatus: Sendable {
    enum Phase: Sendable { case disconnected, connecting, signedIn, refreshing, disconnecting }
    enum SpokenProgressAccess: Sendable { case unknown, notGranted, granted }
    enum PermissionUpdate: Sendable { case requesting, saving }
    var phase: Phase = .disconnected
    var issue: String?
    var canRetrySavedLogin = false
    var retryAvailableAt: Date?
    var spokenProgressAccess: SpokenProgressAccess = .unknown
    var permissionUpdate: PermissionUpdate?
    var busy: Bool { phase == .connecting || phase == .refreshing || phase == .disconnecting }
}
private struct SpotifyRefreshCredential: Codable, Sendable {
    let clientID, refreshToken: String
    let spokenProgressGranted: Bool?
}

@MainActor
final class SpotifyAuthorization {
    private static let defaultRetryDelays: [TimeInterval] = [1, 2, 4, 8, 16, 32, 60]
    private enum DeferredRefresh { case expiry, unauthorized }
    private let credentials: any CredentialStore
    private let client: SpotifyOAuthClient
    private let status: (SpotifyAccountStatus) -> Void
    private let access: (String) -> Void
    private let disconnected: () -> Void
    private let openBrowser: (URL) -> Bool
    private let callbackPort: UInt16?
    private var work: Task<Void, Never>?
    private var expiry: Task<Void, Never>?
    private var retryWork: Task<Void, Never>?
    private let retryDelays: [TimeInterval]
    private var retryIndex = 0
    private var canRetrySavedLogin = false
    private var accountStatus = SpotifyAccountStatus()
    private var deferredRefresh: DeferredRefresh?
    private var callback: LoopbackOAuthCallback?
    private var generation: UInt64 = 0
    private var clientID = ""
    private var bearerRecoveryAttempted = false
    private var wantsConnection = false
    init(credentials: any CredentialStore = KeychainCredentialStore(), client: SpotifyOAuthClient = SpotifyOAuthClient(),
         status: @escaping (SpotifyAccountStatus) -> Void, access: @escaping (String) -> Void, disconnected: @escaping () -> Void,
         openBrowser: @escaping (URL) -> Bool = { NSWorkspace.shared.open($0) }, callbackPort: UInt16? = SpotifyOAuthClient.callbackPort,
         retryDelays: [TimeInterval]? = nil) {
        self.credentials = credentials; self.client = client; self.status = status; self.access = access
        self.disconnected = disconnected; self.openBrowser = openBrowser
        self.callbackPort = callbackPort
        let configuredDelays = retryDelays?.prefix(7).filter { $0.isFinite && $0 > 0 }.map { min(60, max(0.01, $0)) } ?? []
        self.retryDelays = configuredDelays.isEmpty ? Self.defaultRetryDelays : configuredDelays
    }
    static func validClientID(_ value: String) -> Bool { value.utf8.count == 32 && value.utf8.allSatisfy { (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) } }
    func restore(clientID: String) {
        guard Self.validClientID(clientID), work == nil, retryWork == nil else { return }
        cancelWork()
        self.clientID = clientID; bearerRecoveryAttempted = false; wantsConnection = true
        refresh()
    }
    func connect(clientID: String) {
        guard accountStatus.permissionUpdate == nil else { return }
        guard Self.validClientID(clientID) else { publishStatus(.init(issue: SpotifyLoginFailure.invalidClient.localizedDescription)); return }
        cancelWork(); disconnected(); self.clientID = clientID; bearerRecoveryAttempted = false; wantsConnection = true
        requestAuthorization(updatingPermissions: false)
    }
    func updatePermissions() {
        guard accountStatus.phase == .signedIn, wantsConnection, work == nil, retryWork == nil else { return }
        requestAuthorization(updatingPermissions: true)
    }
    private func requestAuthorization(updatingPermissions: Bool) {
        let generation = self.generation
        let clientID = self.clientID
        if updatingPermissions {
            var pending = accountStatus
            pending.issue = nil; pending.permissionUpdate = .requesting
            publishStatus(pending)
        } else { publishStatus(.init(phase: .connecting)) }
        work = Task { [weak self] in
            guard let self, generation == self.generation, !Task.isCancelled else { return }
            do {
                let pkce = try SpotifyPKCE()
                let callback = LoopbackOAuthCallback(state: pkce.state, port: callbackPort); self.callback = callback
                let redirect = try await callback.start()
                guard generation == self.generation, !Task.isCancelled else { callback.cancel(); return }
                let authorization = pkce.authorizationURL(clientID: clientID, redirect: redirect, showDialog: updatingPermissions)
                guard openBrowser(authorization) else { throw SpotifyLoginFailure.network }
                let code = try await callback.code()
                guard generation == self.generation, !Task.isCancelled else { return }
                let token = try await client.exchange(clientID: clientID, code: code, verifier: pkce.verifier, redirect: redirect)
                guard generation == self.generation, !Task.isCancelled else { return }
                guard let refresh = token.refreshToken, !refresh.isEmpty else { throw SpotifyLoginFailure.authorization }
                if updatingPermissions {
                    var saving = accountStatus; saving.permissionUpdate = .saving
                    publishStatus(saving)
                }
                try await accept(token, refresh: refresh, replacingAuthorization: updatingPermissions, generation: generation)
            } catch {
                guard generation == self.generation, !Task.isCancelled else { return }
                if updatingPermissions { finishPermissionUpdate(issue: error.localizedDescription); return }
                self.callback?.cancel(); self.callback = nil; work = nil
                wantsConnection = false
                publishStatus(.init(issue: error.localizedDescription))
            }
        }
    }
    func cancel() {
        if accountStatus.phase == .signedIn { cancelPermissionRequest(); return }
        wantsConnection = false; cancelWork(); disconnected(); publishStatus(.init())
    }
    private func cancelPermissionRequest() {
        guard accountStatus.permissionUpdate == .requesting else { return }
        work?.cancel()
        finishPermissionUpdate()
    }
    private func finishPermissionUpdate(issue: String? = nil) {
        callback?.cancel(); callback = nil; work = nil
        let pendingRefresh = deferredRefresh; deferredRefresh = nil
        var restored = accountStatus
        restored.permissionUpdate = nil; restored.issue = issue
        publishStatus(restored)
        switch pendingRefresh {
        case .expiry: refresh()
        case .unauthorized: unauthorized()
        case nil: break
        }
    }
    private func publishStatus(_ value: SpotifyAccountStatus) {
        accountStatus = value
        status(value)
    }
    func noteConnected() { bearerRecoveryAttempted = false }
    func retrySavedLogin() {
        guard wantsConnection, canRetrySavedLogin, work == nil, retryWork == nil else { return }
        retryIndex = 0; canRetrySavedLogin = false
        refresh()
    }
    func unauthorized() {
        if accountStatus.permissionUpdate == .saving { deferredRefresh = .unauthorized; return }
        cancelPermissionRequest()
        guard wantsConnection, work == nil, retryWork == nil, !canRetrySavedLogin else { return }
        guard !bearerRecoveryAttempted else {
            wantsConnection = false; cancelWork(); disconnected()
            publishStatus(.init(issue: SpotifyLoginFailure.invalidGrant.localizedDescription)); return
        }
        bearerRecoveryAttempted = true; refresh()
    }
    private func refresh(recoveryIssue: String? = nil) {
        if accountStatus.permissionUpdate == .saving {
            if deferredRefresh == nil { deferredRefresh = .expiry }
            return
        }
        cancelPermissionRequest()
        guard wantsConnection, work == nil, retryWork == nil, Self.validClientID(clientID) else { return }
        let generation = self.generation
        work = Task { [weak self] in
            guard let self, generation == self.generation, !Task.isCancelled else { return }
            var foundSavedLogin = false
            do {
                guard let data = try await credentials.read(account: "spotify"), let stored = try? JSONDecoder().decode(SpotifyRefreshCredential.self, from: data), stored.clientID == clientID else {
                    if generation == self.generation {
                        wantsConnection = false; work = nil
                        if recoveryIssue != nil { disconnected() }
                        publishStatus(.init())
                    }
                    return
                }
                guard generation == self.generation, !Task.isCancelled else { return }
                foundSavedLogin = true
                let token = try await client.refresh(clientID: clientID, refreshToken: stored.refreshToken)
                guard generation == self.generation, !Task.isCancelled else { return }
                try await accept(token, refresh: token.refreshToken ?? stored.refreshToken,
                                 previousSpokenProgressGrant: stored.spokenProgressGranted, generation: generation)
            } catch {
                guard generation == self.generation, !Task.isCancelled else { return }
                if case SpotifyLoginFailure.invalidGrant = error { try? await credentials.remove(account: "spotify") }
                guard generation == self.generation, !Task.isCancelled else { return }
                work = nil; expiry?.cancel(); expiry = nil
                if foundSavedLogin, let failure = (error as? SpotifyLoginFailure)?.refreshRetry {
                    scheduleRetry(after: failure.delay, issue: failure.message, generation: generation)
                } else if !foundSavedLogin, let failure = error as? CredentialFailure,
                          failure.status == errSecInteractionNotAllowed || failure.status == errSecUserCanceled {
                    canRetrySavedLogin = true
                    publishStatus(.init(issue: failure.localizedDescription, canRetrySavedLogin: true))
                } else {
                    wantsConnection = false; disconnected(); publishStatus(.init(issue: error.localizedDescription))
                }
            }
        }
        publishStatus(.init(phase: .refreshing, issue: recoveryIssue))
    }
    private func scheduleRetry(after minimumDelay: TimeInterval, issue: String, generation: UInt64) {
        let delay = max(minimumDelay, retryDelays[retryIndex] * Double.random(in: 0.8...1))
        retryIndex = min(retryIndex + 1, retryDelays.count - 1)
        retryWork = Task { [weak self] in
            do {
                // Chunk unusually long server delays so conversion to Duration stays bounded.
                var remaining = delay
                while remaining > 86_400 {
                    try await Task.sleep(for: .seconds(86_400))
                    remaining -= 86_400
                }
                try await Task.sleep(for: .seconds(remaining))
            } catch { return }
            guard let self, self.generation == generation, wantsConnection, !Task.isCancelled else { return }
            retryWork = nil; refresh(recoveryIssue: issue)
        }
        publishStatus(.init(phase: .refreshing, issue: issue))
    }
    private func accept(_ token: SpotifyTokenReply, refresh: String,
                        previousSpokenProgressGrant: Bool? = nil, replacingAuthorization: Bool = false,
                        generation: UInt64) async throws {
        let spokenProgressGrant = token.spokenProgressGranted ?? previousSpokenProgressGrant
        let data = try JSONEncoder().encode(SpotifyRefreshCredential(clientID: clientID, refreshToken: refresh,
                                                                     spokenProgressGranted: spokenProgressGrant))
        try await credentials.write(data, account: "spotify")
        guard generation == self.generation, !Task.isCancelled else { return }
        // The browser can return a different account. Retire the old provider
        // generation only once the replacement login is safely stored.
        if replacingAuthorization { bearerRecoveryAttempted = false; disconnected() }
        deferredRefresh = nil
        callback = nil; work = nil; retryWork?.cancel(); retryWork = nil
        retryIndex = 0; canRetrySavedLogin = false
        let progressAccess: SpotifyAccountStatus.SpokenProgressAccess = switch spokenProgressGrant {
        case true?: .granted
        case false?: .notGranted
        case nil: .unknown
        }
        publishStatus(.init(phase: .signedIn, spokenProgressAccess: progressAccess)); access(token.accessToken)
        expiry?.cancel()
        expiry = Task { [weak self] in
            do { try await Task.sleep(for: .seconds(max(1, token.expiresIn - 60))) } catch { return }
            guard let self, generation == self.generation else { return }; self.refresh()
        }
    }
    func disconnect() {
        let pendingWrite = accountStatus.permissionUpdate == .saving ? work : nil
        wantsConnection = false; cancelWork(); disconnected(); publishStatus(.init(phase: .disconnecting))
        let generation = self.generation
        work = Task { [weak self] in
            guard let self else { return }
            await pendingWrite?.value
            guard generation == self.generation, !Task.isCancelled else { return }
            do { try await credentials.remove(account: "spotify"); if generation == self.generation { publishStatus(.init()); work = nil } }
            catch { if generation == self.generation { publishStatus(.init(issue: error.localizedDescription)); work = nil } }
        }
    }
    private func cancelWork() {
        generation &+= 1; work?.cancel(); work = nil; expiry?.cancel(); expiry = nil; callback?.cancel(); callback = nil
        retryWork?.cancel(); retryWork = nil; retryIndex = 0; canRetrySavedLogin = false
        deferredRefresh = nil
    }
    func shutdown() { wantsConnection = false; cancelWork() }
}
