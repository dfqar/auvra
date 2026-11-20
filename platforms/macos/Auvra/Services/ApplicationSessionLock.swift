import Darwin
import Foundation

/// One process can open a user's playback state. The kernel releases the
/// descriptor on exit, so the lock file itself stays in place across launches.
final class ApplicationSessionLock {
    enum Admission {
        case primary(ApplicationSessionLock)
        case occupied(pid_t?)
    }

    private let descriptor: Int32
    private init(_ descriptor: Int32) { self.descriptor = descriptor }
    deinit { close(descriptor) }

    static func acquire(in directory: URL) throws -> Admission {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        let path = directory.appendingPathComponent(".auvra-session.lock").path
        let fd = open(path, O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK, 0o600)
        guard fd >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        var acquired = false
        defer { if !acquired { close(fd) } }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
              info.st_uid == geteuid(), info.st_nlink == 1 else { throw POSIXError(.EACCES) }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            guard errno == EWOULDBLOCK else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
            var buffer = [UInt8](repeating: 0, count: 16)
            let count = pread(fd, &buffer, buffer.count, 0)
            let pid = count > 0 ? pid_t(String(decoding: buffer.prefix(count), as: UTF8.self)) : nil
            return .occupied(pid.flatMap { $0 > 0 ? $0 : nil })
        }
        let pid = Array(String(getpid()).utf8)
        guard ftruncate(fd, 0) == 0, pid.withUnsafeBytes({ pwrite(fd, $0.baseAddress, $0.count, 0) }) == pid.count else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        acquired = true
        return .primary(ApplicationSessionLock(fd))
    }
}
