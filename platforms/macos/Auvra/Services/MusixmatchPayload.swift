import Foundation

/// Decodes the lyric bodies of a verified Musixmatch recording.
enum MusixmatchPayload {
    private static let maxPayloadBytes = 512 * 1024
    private static let maxLines = 3_000
    private static let maxWords = 30_000
    private static let maxDurationMs: UInt64 = 86_400_000

    // The client uses seconds, including fractional values. A malformed time
    // should not prevent us from reading the lyric text in the same record.
    private struct Seconds: Decodable {
        let value: Double?

        init(from decoder: any Decoder) throws {
            let container = try decoder.singleValueContainer()
            value = (try? container.decode(Double.self)) ?? (try? container.decode(String.self)).flatMap(Double.init)
        }
    }

    private struct RichLine: Decodable {
        struct Token: Decodable {
            let c: String
            let o: Seconds?
        }

        let ts, te: Seconds?
        let l: [Token]?
        let x: String?

        var text: String {
            if let x, !x.isEmpty { return x }
            return (l ?? []).map(\.c).joined()
        }
    }

    private struct Subtitle: Decodable {
        struct Time: Decodable { let total: Seconds? }
        let text: String?
        let time: Time?
    }

    private struct Cue {
        let text: String
        let start: UInt64?
        var end: UInt64? = nil
        var words: [LyricsWord] = []
    }

    static func document(richsync: String?, subtitle: String?, plain: String?, durationMs: UInt64) -> LyricsDocument? {
        guard (1...maxDurationMs).contains(durationMs) else { return nil }
        let rich = decode([RichLine].self, from: richsync).flatMap { richDocument($0, duration: durationMs) }
        if rich?.precision == "word" { return rich }

        let synced = subtitleDocument(subtitle, duration: durationMs)
        if synced?.hasTiming == true { return synced }
        if rich?.hasTiming == true { return rich }
        return parsedText(plain, format: "plain", duration: durationMs) ?? synced ?? rich
    }

    private static func richDocument(_ rows: [RichLine], duration: UInt64) -> LyricsDocument? {
        guard rows.count <= maxLines else { return nil }
        let cues = rows.map { row in
            let text = row.text
            let start = cueStart(row.ts?.value, text: text, duration: duration)
            let end = milliseconds(row.te?.value, maximum: duration).flatMap { end in
                start.flatMap { end > $0 ? end : nil }
            }
            var cue = Cue(text: text, start: start, end: end)
            if let start, let end {
                cue.words = words(in: row, text: text, start: start, end: end)
            }
            return cue
        }
        return document(from: cues, duration: duration)
    }

    private static func words(in row: RichLine, text: String, start: UInt64, end: UInt64) -> [LyricsWord] {
        guard let lineTime = row.ts?.value, let tokens = row.l, !tokens.isEmpty, tokens.count <= 1_000,
              tokens.map(\.c).joined() == text else { return [] }
        var starts: [UInt64] = []
        starts.reserveCapacity(tokens.count)
        for token in tokens {
            guard !token.c.isEmpty, token.c.utf8.count <= 2_048, let offset = token.o?.value,
                  offset >= 0, let time = milliseconds(lineTime + offset, maximum: end),
                  time >= start, time < end, starts.last.map({ time > $0 }) ?? true else { return [] }
            starts.append(time)
        }
        return tokens.enumerated().map { index, token in
            // Richsync provides starts. The following onset or the source's
            // line boundary bounds the current word.
            let wordEnd = index + 1 < starts.count ? starts[index + 1] : end
            return LyricsWord(text: token.c, startMs: starts[index], endMs: wordEnd, endEstimated: true)
        }
    }

    private static func subtitleDocument(_ payload: String?, duration: UInt64) -> LyricsDocument? {
        if let rows = decode([Subtitle].self, from: payload) {
            guard rows.count <= maxLines else { return nil }
            let cues = rows.map { row in
                let text = row.text ?? ""
                return Cue(text: text, start: cueStart(row.time?.total?.value, text: text, duration: duration))
            }
            return document(from: cues, duration: duration)
        }
        // Some client profiles return LRC instead of a JSON subtitle body.
        guard let payload, payload.trimmingCharacters(in: .whitespacesAndNewlines).hasPrefix("["),
              let parsed = parsedText(payload, format: "lrc", duration: duration), parsed.hasTiming else { return nil }
        return parsed
    }

    private static func document(from cues: [Cue], duration: UInt64) -> LyricsDocument? {
        let lines: [LyricsLine]
        let precision: String
        if cues.allSatisfy({ $0.start != nil || $0.text.isEmpty }) {
            let sorted = cues.filter { $0.start != nil }.sorted { ($0.start ?? 0) < ($1.start ?? 0) }
            lines = sorted.enumerated().map { index, cue in
                let next = index + 1 < sorted.count ? sorted[index + 1].start : nil
                return LyricsLine(text: cue.text, startMs: cue.start, endMs: cue.end ?? next ?? duration, words: cue.words)
            }
            precision = cues.contains(where: { !$0.words.isEmpty }) ? "word" : "line"
        } else {
            // With a missing line start, keep the source text in order. The
            // caller can still prefer a separate, intact timed body.
            lines = cues.map { LyricsLine(text: $0.text, startMs: nil, endMs: nil, words: []) }
            precision = "plain"
        }
        let result = LyricsDocument(lines: lines, instrumental: false, precision: precision)
        let wordCount = lines.reduce(0) { $0 + $1.words.count }
        guard wordCount <= maxWords, result.isValid, result.hasTranslatableText,
              !lines.contains(where: { $0.text.contains("\0") }) else { return nil }
        return result
    }

    private static func cueStart(_ seconds: Double?, text: String, duration: UInt64) -> UInt64? {
        guard let time = milliseconds(seconds, maximum: duration),
              time < duration || text.isEmpty else { return nil }
        return time
    }

    private static func milliseconds(_ seconds: Double?, maximum: UInt64) -> UInt64? {
        guard let seconds, seconds.isFinite, seconds >= 0 else { return nil }
        let value = (seconds * 1_000).rounded()
        guard value <= Double(maximum) else { return nil }
        return UInt64(value)
    }

    private static func decode<T: Decodable>(_ type: T.Type, from payload: String?) -> T? {
        guard let payload, payload.utf8.count <= maxPayloadBytes else { return nil }
        return try? JSONDecoder().decode(type, from: Data(payload.utf8))
    }

    private static func parsedText(_ text: String?, format: String, duration: UInt64) -> LyricsDocument? {
        guard let text, text.utf8.count <= maxPayloadBytes,
              let result = try? LyricsService.parse(text, format: format, duration: duration),
              result.isValid, result.hasTranslatableText, result.lines.allSatisfy({ line in
                  (line.startMs.map { $0 < duration || ($0 == duration && !line.hasText) } ?? true)
                      && (line.endMs.map { $0 <= duration } ?? true)
              }) else { return nil }
        return result
    }
}
