import Darwin
import Foundation
import Security

enum OfflineLyricsTranslation {
    private static let worker = DispatchQueue(label: "app.auvra.offline-lyrics", qos: .userInitiated)
    private static let maximumOutput = 6 * 96 * 1024 + 32 * 1024

    private enum Failure: Error { case unavailable, invalidInput, invalidOutput, failed, timedOut }

    private struct Route: Hashable, Sendable {
        let source, target: String
        let models: [String]
    }

    private static let routes = [Route(source: "ja", target: "en", models: ["jaen"]),
                                 Route(source: "en", target: "ru", models: ["enru"]),
                                 Route(source: "ja", target: "ru", models: ["jaen", "enru"])]
    // The optional pack is installed inside the signed bundle. Its UI availability lasts for this launch.
    private static let installedRoutes = Set(routes.filter { executable(for: $0) != nil })

    private struct Request: Encodable {
        let source, target: String
        let groups: [[String]]
    }

    private struct Response: Decodable { let groups: [[String]] }

    private final class Cancellation: @unchecked Sendable {
        private let lock = NSLock()
        private var cancelled = false
        func cancel() { lock.withLock { cancelled = true } }
        func check() throws {
            if lock.withLock({ cancelled }) { throw CancellationError() }
        }
    }

    static func isAvailable(source: Locale.Language, target: Locale.Language) -> Bool {
        guard let route = route(from: source, to: target) else { return false }
        return installedRoutes.contains(route)
    }

    static func hasPack(target: Locale.Language) -> Bool {
        guard let target = target.languageCode?.identifier else { return false }
        return installedRoutes.contains { $0.target == target }
    }

    static func translate(_ groups: [[String]], source: Locale.Language, target: Locale.Language) async throws -> [[String]] {
        guard let route = route(from: source, to: target) else { throw Failure.unavailable }
        let request = Request(source: route.source, target: route.target, groups: groups)
        let cancellation = Cancellation()
        return try await withTaskCancellationHandler {
            try Task.checkCancellation()
            let result: [[String]] = try await withCheckedThrowingContinuation { continuation in
                worker.async {
                    continuation.resume(with: Result { try run(request, route: route, cancellation: cancellation) })
                }
            }
            try Task.checkCancellation()
            return result
        } onCancel: {
            cancellation.cancel()
        }
    }

    private static func route(from source: Locale.Language, to target: Locale.Language) -> Route? {
        guard let source = source.languageCode?.identifier, let target = target.languageCode?.identifier else { return nil }
        return routes.first { $0.source == source && $0.target == target }
    }

    private static func executable(for route: Route) -> URL? {
        let bundle = Bundle.main.bundleURL.resolvingSymlinksInPath().standardizedFileURL
        let helper = bundle.appendingPathComponent("Contents/Helpers/AuvraLyricsHost")
        let assets = bundle.appendingPathComponent("Contents/Resources/OfflineLyrics", isDirectory: true)
        let files = ["bergamot.js", "bergamot.wasm"] + route.models.flatMap { pair in
            ["model", "lex", "vocab"].map { "\(pair)-\($0).bin" }
        }
        guard FileManager.default.isExecutableFile(atPath: helper.path), isRegularFile(helper),
              files.allSatisfy({ isRegularFile(assets.appendingPathComponent($0)) }) else { return nil }
        return helper
    }

    private static func isRegularFile(_ url: URL) -> Bool {
        guard url.resolvingSymlinksInPath().standardizedFileURL == url.standardizedFileURL,
              let values = try? url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey]) else { return false }
        return values.isRegularFile == true && (values.fileSize ?? 0) > 0
    }

    private static func validateSeal(_ helper: URL) throws {
        var running: SecCode?, app: SecStaticCode?, child: SecStaticCode?
        let strict = SecCSFlags(rawValue: UInt32(kSecCSStrictValidate | kSecCSCheckAllArchitectures))
        // The running bundle's resource seal binds the exact helper and its model files, including in ad-hoc builds.
        guard SecCodeCopySelf([], &running) == errSecSuccess, let running,
              SecCodeCopyStaticCode(running, [], &app) == errSecSuccess, let app,
              SecStaticCodeCheckValidity(app, strict, nil) == errSecSuccess,
              SecStaticCodeCreateWithPath(helper as CFURL, [], &child) == errSecSuccess, let child,
              SecStaticCodeCheckValidity(child, strict, nil) == errSecSuccess else { throw Failure.unavailable }
    }

    private static func bounded(_ groups: [[String]], lineBytes: Int, totalBytes: Int) -> Bool {
        guard (1...400).contains(groups.count) else { return false }
        var rows = 0, bytes = 0
        for group in groups {
            guard (1...4).contains(group.count) else { return false }
            rows += group.count
            for line in group {
                let size = line.utf8.count
                bytes += size
                guard rows <= 400, size <= lineBytes, bytes <= totalBytes,
                      !line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return false }
            }
        }
        return true
    }

    private static func run(_ request: Request, route: Route, cancellation: Cancellation) throws -> [[String]] {
        try cancellation.check()
        guard bounded(request.groups, lineBytes: 4096, totalBytes: 48 * 1024) else { throw Failure.invalidInput }
        guard let executable = executable(for: route) else { throw Failure.unavailable }
        try validateSeal(executable)
        let payload = try JSONEncoder().encode(request)
        guard payload.count <= 6 * 48 * 1024 + 16 * 1024 else { throw Failure.invalidInput }
        try cancellation.check()

        let child = Process(), incoming = Pipe(), outgoing = Pipe()
        child.executableURL = executable
        child.environment = [:]
        child.standardInput = incoming
        child.standardOutput = outgoing
        child.standardError = FileHandle.nullDevice
        let input = incoming.fileHandleForWriting, output = outgoing.fileHandleForReading
        try setNonblocking(input.fileDescriptor)
        try setNonblocking(output.fileDescriptor)
        guard fcntl(input.fileDescriptor, F_SETNOSIGPIPE, 1) == 0 else { throw Failure.failed }
        try child.run()
        defer {
            // Error and cancellation cleanup must also stop a helper stalled in WebKit.
            if child.isRunning, child.processIdentifier > 0 { _ = Darwin.kill(child.processIdentifier, SIGKILL) }
            child.waitUntilExit()
            try? input.close()
            try? output.close()
        }
        let result = try exchange(payload, input: input, output: output, child: child, cancellation: cancellation)
        if child.terminationReason == .exit, child.terminationStatus == 4 { throw Failure.timedOut }
        guard child.terminationReason == .exit, child.terminationStatus == 0 else { throw Failure.failed }
        guard let response = try? JSONDecoder().decode(Response.self, from: result),
              response.groups.map(\.count) == request.groups.map(\.count),
              bounded(response.groups, lineBytes: 8192, totalBytes: 96 * 1024) else { throw Failure.invalidOutput }
        return response.groups
    }

    private static func setNonblocking(_ descriptor: Int32) throws {
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else { throw Failure.failed }
    }

    private static func exchange(_ payload: Data, input: FileHandle, output: FileHandle, child: Process,
                                 cancellation: Cancellation) throws -> Data {
        let deadline = ContinuousClock.now.advanced(by: .seconds(35))
        var descriptors = [pollfd(fd: input.fileDescriptor, events: Int16(POLLOUT), revents: 0),
                           pollfd(fd: output.fileDescriptor, events: Int16(POLLIN), revents: 0)]
        var sent = 0, result = Data()
        var buffer = [UInt8](repeating: 0, count: 8192)
        while descriptors[1].fd >= 0 || child.isRunning {
            try cancellation.check()
            guard ContinuousClock.now < deadline else { throw Failure.timedOut }
            let ready = descriptors.withUnsafeMutableBufferPointer { Darwin.poll($0.baseAddress, nfds_t($0.count), 25) }
            if ready < 0 && errno != EINTR { throw Failure.failed }
            guard ready > 0 else { continue }
            if descriptors[0].revents != 0 {
                let written = payload.withUnsafeBytes { bytes in
                    Darwin.write(descriptors[0].fd, bytes.baseAddress!.advanced(by: sent), bytes.count - sent)
                }
                if written > 0 { sent += written }
                else if written < 0 && errno != EINTR && errno != EAGAIN { throw Failure.failed }
                if sent == payload.count {
                    try input.close()
                    descriptors[0].fd = -1
                }
            }
            if descriptors[1].revents != 0 {
                let count = buffer.withUnsafeMutableBytes { Darwin.read(descriptors[1].fd, $0.baseAddress, $0.count) }
                if count > 0 {
                    guard result.count + count <= maximumOutput else { throw Failure.invalidOutput }
                    result.append(contentsOf: buffer.prefix(count))
                } else if count == 0 {
                    descriptors[1].fd = -1
                } else if errno != EINTR && errno != EAGAIN {
                    throw Failure.failed
                }
            }
        }
        guard sent == payload.count else { throw Failure.failed }
        return result
    }
}
