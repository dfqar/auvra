import AppKit
import ImageIO
import Foundation

struct ArtworkPalette: Sendable, Equatable { let colors: [[Float]] }
struct DecodedArtwork: @unchecked Sendable { let image: CGImage; var palette: ArtworkPalette? = nil }
private final class ArtworkCacheEntry {
    let image: NSImage
    let palette: ArtworkPalette?
    init(_ decoded: DecodedArtwork) {
        image = NSImage(cgImage: decoded.image, size: NSSize(width: decoded.image.width, height: decoded.image.height))
        palette = decoded.palette
    }
}

/// Serial decode bounds the number of concurrent image decoders independently of grid size.
private actor ArtworkDecodeWorker {
    func decode(_ reference: String) -> DecodedArtwork? {
        let url = URL(fileURLWithPath: reference)
        guard let size = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize,
              size <= 16 * 1024 * 1024,
              let source = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary) else { return nil }
        return decode(source)
    }
    func decode(data: Data) -> DecodedArtwork? {
        guard data.count <= 16 * 1024 * 1024, let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary) else { return nil }
        return decode(source)
    }
    private func decode(_ source: CGImageSource) -> DecodedArtwork? {
        guard let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? NSNumber,
              let height = properties[kCGImagePropertyPixelHeight] as? NSNumber,
              width.doubleValue > 0, height.doubleValue > 0,
              width.doubleValue * height.doubleValue <= 64_000_000,
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceThumbnailMaxPixelSize: 512,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceShouldCacheImmediately: true
              ] as CFDictionary) else { return nil }
        return DecodedArtwork(image: image, palette: Self.palette(image))
    }
    /// Tiny sRGB histogram on the decode worker; never runs on a display/audio callback.
    private static func palette(_ image: CGImage) -> ArtworkPalette? {
        var pixels = [UInt8](repeating: 0, count: 32 * 32 * 4)
        let drawn = pixels.withUnsafeMutableBytes { bytes -> Bool in
            guard let context = CGContext(data: bytes.baseAddress, width: 32, height: 32, bitsPerComponent: 8, bytesPerRow: 128, space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue) else { return false }
            context.draw(image, in: CGRect(x: 0, y: 0, width: 32, height: 32)); return true
        }
        guard drawn else { return nil }
        var bins: [Int: (weight: Float, sum: [Float])] = [:]
        for offset in stride(from: 0, to: pixels.count, by: 4) {
            let alpha = Float(pixels[offset + 3]) / 255
            guard alpha > 0.5 else { continue }
            let rgb = (0..<3).map { min(1, Float(pixels[offset + $0]) / 255 / alpha) }
            let high = rgb.max()!, low = rgb.min()!
            let weight = 0.08 + (high - low) * max(0.1, 1 - abs(high - 0.6))
            let bin = (Int(rgb[0] * 15) << 8) | (Int(rgb[1] * 15) << 4) | Int(rgb[2] * 15)
            var value = bins[bin] ?? (0, [0,0,0]); value.weight += weight
            for j in 0..<3 { value.sum[j] += rgb[j] * weight }; bins[bin] = value
        }
        var colors: [[Float]] = []
        for (_, value) in bins.sorted(by: { $0.value.weight == $1.value.weight ? $0.key < $1.key : $0.value.weight > $1.value.weight }) {
            let color = value.sum.map { $0 / value.weight }
            if colors.allSatisfy({ existing in zip(existing, color).map { abs($0 - $1) }.reduce(0,+) > 0.35 }) { colors.append(color) }
            if colors.count == 3 { break }
        }
        guard let first = colors.first else { return nil }
        while colors.count < 3 { colors.append(first) }
        return ArtworkPalette(colors: colors)
    }
}

enum RemoteArtworkURL {
    static func validated(_ reference: String) -> URL? {
        guard reference.utf8.count <= 4096, let url = URL(string: reference), url.scheme == "https",
              url.user == nil, url.password == nil, url.port == nil else { return nil }
        switch url.host {
        case "i.scdn.co", "mosaic.scdn.co": return url
        case "image-cdn-fa.spotifycdn.com", "image-cdn-ak.spotifycdn.com": return url.path.hasPrefix("/image/") ? url : nil
        default: return nil
        }
    }
}

private final class NoArtworkRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}
private actor RemoteArtworkFetchWorker {
    private var active = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private func acquire() async {
        if active < 4 { active += 1; return }
        await withCheckedContinuation { waiters.append($0) }
    }
    private func release() {
        if waiters.isEmpty { active -= 1 } else { waiters.removeFirst().resume() }
    }
    private let session: URLSession = {
        let config = URLSessionConfiguration.ephemeral; config.urlCache = nil; config.httpCookieStorage = nil
        return URLSession(configuration: config, delegate: NoArtworkRedirects(), delegateQueue: nil)
    }()
    func fetch(_ reference: String) async -> Data? {
        guard let url = RemoteArtworkURL.validated(reference) else { return nil }
        await acquire(); defer { release() }
        guard !Task.isCancelled else { return nil }
        do {
            var request = URLRequest(url: url); request.timeoutInterval = 15
            let (bytes, response) = try await session.bytes(for: request)
            guard let http = response as? HTTPURLResponse, http.statusCode == 200, response.url?.host == url.host, response.expectedContentLength <= 16 * 1024 * 1024 else { return nil }
            var data = Data()
            for try await byte in bytes {
                if Task.isCancelled || data.count >= 16 * 1024 * 1024 { return nil }
                data.append(byte)
            }
            return data
        } catch { return nil }
    }
}

/// Bounded local/CDN references. IO and thumbnail decode belong to separate actors.
@MainActor
final class LocalArtworkLoader {
    static let shared = LocalArtworkLoader()
    private let cache = NSCache<NSString, ArtworkCacheEntry>()
    private let worker = ArtworkDecodeWorker()
    private let remote = RemoteArtworkFetchWorker()
    private struct Request { let id: UUID; let task: Task<DecodedArtwork?, Never> }
    private var inFlight: [String: Request] = [:]
    // Cancelled requests keep their slot until their operation actually finishes.
    // Repeated Disconnect cannot accumulate unbounded cancelled fetch waiters.
    private var outstanding = 0
    private var prewarming = false
    private let operation: (@Sendable (String) async -> DecodedArtwork?)?
    init(operation: (@Sendable (String) async -> DecodedArtwork?)? = nil) {
        self.operation = operation; cache.countLimit = 128; cache.totalCostLimit = 32 * 1024 * 1024
    }
    func image(for reference: String) async -> NSImage? {
        await entry(for: reference)?.image
    }
    func palette(for reference: String) async -> ArtworkPalette? { await entry(for: reference)?.palette }
    /// A single speculative cover can share its fetch with later on-screen demand.
    func prewarm(_ reference: String) async {
        guard !prewarming, !Task.isCancelled, RemoteArtworkURL.validated(reference) != nil,
              cache.object(forKey: reference as NSString) == nil, inFlight[reference] == nil else { return }
        prewarming = true
        defer { prewarming = false }
        _ = await entry(for: reference)
    }
    private func entry(for reference: String) async -> ArtworkCacheEntry? {
        if let entry = cache.object(forKey: reference as NSString) { return entry }
        guard reference.utf8.count <= 4096, reference.hasPrefix("/") || reference.hasPrefix("https://") else { return nil }
        let request: Request
        let created: Bool
        if let existing = inFlight[reference] { request = existing; created = false }
        else {
            guard outstanding < 64 else { return nil }
            outstanding += 1; created = true
            let worker = self.worker; let remote = self.remote; let operation = self.operation
            let task = Task {
                if let operation { return await operation(reference) }
                if reference.hasPrefix("/") { return await worker.decode(reference) }
                guard let data = await remote.fetch(reference) else { return nil }
                return await worker.decode(data: data)
            }
            request = Request(id: UUID(), task: task)
            inFlight[reference] = request
        }
        let decoded = await request.task.value
        if created { outstanding -= 1 }
        if inFlight[reference]?.id == request.id { inFlight.removeValue(forKey: reference) }
        guard let decoded, !request.task.isCancelled else { return nil }
        let entry = ArtworkCacheEntry(decoded)
        cache.setObject(entry, forKey: reference as NSString, cost: decoded.image.bytesPerRow * decoded.image.height)
        return Task.isCancelled ? nil : entry
    }
    func discardRemote() {
        for key in inFlight.keys.filter({ $0.hasPrefix("https://") }) { inFlight.removeValue(forKey: key)?.task.cancel() }
        cache.removeAllObjects()
    }
}
