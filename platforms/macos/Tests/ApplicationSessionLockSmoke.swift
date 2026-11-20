import Darwin
import Foundation

@main
enum ApplicationSessionLockSmoke {
    private struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    static func main() throws {
        if CommandLine.arguments.count == 3, CommandLine.arguments[1] == "--hold" {
            let lease = try acquire(URL(fileURLWithPath: CommandLine.arguments[2]))
            withExtendedLifetime(lease) {
                FileHandle.standardOutput.write(Data([1]))
                _ = FileHandle.standardInput.readDataToEndOfFile()
            }
            return
        }

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("auvra-launch-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let profile = directory.appendingPathComponent("profile", isDirectory: true)
        try exerciseChild(in: profile, crash: false)
        try exerciseChild(in: profile, crash: true)

        let invalidProfile = directory.appendingPathComponent("invalid-profile", isDirectory: true)
        try FileManager.default.createDirectory(at: invalidProfile, withIntermediateDirectories: false)
        let sentinel = directory.appendingPathComponent("sentinel")
        let original = Data("keep this file".utf8)
        try original.write(to: sentinel)
        try FileManager.default.createSymbolicLink(
            at: invalidProfile.appendingPathComponent(".auvra-session.lock"), withDestinationURL: sentinel)
        do {
            _ = try ApplicationSessionLock.acquire(in: invalidProfile)
            throw Failure(description: "Accepted a symlink as the session lock")
        } catch let error as POSIXError where error.code == .ELOOP {}
        guard try Data(contentsOf: sentinel) == original else {
            throw Failure(description: "Session lock modified the symlink target")
        }
        print("PASS: competing processes, separate profiles, normal and crash recovery, symlink rejection")
    }

    private static func exerciseChild(in directory: URL, crash: Bool) throws {
        let child = Process()
        child.executableURL = URL(fileURLWithPath: CommandLine.arguments[0]).absoluteURL
        child.arguments = ["--hold", directory.path]
        let ready = Pipe()
        let release = Pipe()
        child.standardOutput = ready
        child.standardInput = release
        try child.run()
        defer {
            if child.isRunning {
                _ = kill(child.processIdentifier, SIGKILL)
                child.waitUntilExit()
            }
        }
        guard try ready.fileHandleForReading.read(upToCount: 1) == Data([1]) else {
            throw Failure(description: "Child failed to acquire the session lock")
        }
        guard case .occupied(let owner) = try ApplicationSessionLock.acquire(in: directory),
              owner == child.processIdentifier else {
            throw Failure(description: "Second process did not find the active owner")
        }
        let independent = try acquire(directory.deletingLastPathComponent().appendingPathComponent("independent"))

        if crash {
            guard kill(child.processIdentifier, SIGKILL) == 0 else {
                throw Failure(description: "Could not simulate a crash in the test child")
            }
        } else {
            try release.fileHandleForWriting.close()
        }
        child.waitUntilExit()
        guard crash ? child.terminationReason == .uncaughtSignal : child.terminationStatus == 0 else {
            throw Failure(description: "Child exited unexpectedly")
        }
        let replacement = try acquire(directory)
        withExtendedLifetime((independent, replacement)) {}
    }

    private static func acquire(_ directory: URL) throws -> ApplicationSessionLock {
        guard case .primary(let lease) = try ApplicationSessionLock.acquire(in: directory) else {
            throw Failure(description: "Profile already locked")
        }
        return lease
    }
}
