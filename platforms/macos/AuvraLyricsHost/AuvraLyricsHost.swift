import AppKit
import Darwin
import Foundation
import WebKit

private struct Job: Decodable {
    let source: String
    let target: String
    let groups: [[String]]

    var models: [String] {
        switch (source, target) {
        case ("ja", "en"): ["jaen"]
        case ("en", "ru"): ["enru"]
        case ("ja", "ru"): ["jaen", "enru"]
        default: []
        }
    }

    static func read() throws -> Self {
        let limit = 384 * 1024
        var input = Data()
        while let bytes = try FileHandle.standardInput.read(upToCount: limit + 1 - input.count), !bytes.isEmpty {
            input.append(bytes)
            guard input.count <= limit else { throw HostFailure.invalidInput }
        }
        guard String(data: input, encoding: .utf8) != nil else { throw HostFailure.invalidInput }
        let job = try JSONDecoder().decode(Self.self, from: input)
        guard !job.models.isEmpty, !job.groups.isEmpty,
              job.groups.allSatisfy({ !$0.isEmpty }),
              Self.valid(job.groups, lineLimit: 4096, byteLimit: 48 * 1024) else {
            throw HostFailure.invalidInput
        }
        return job
    }

    func accepts(_ result: [[String]]) -> Bool {
        result.count == groups.count
            && zip(result, groups).allSatisfy { $0.count == $1.count }
            && Self.valid(result, lineLimit: 8192, byteLimit: 96 * 1024)
    }

    private static func valid(_ groups: [[String]], lineLimit: Int, byteLimit: Int) -> Bool {
        var lines = 0
        var bytes = 0
        for group in groups {
            for line in group {
                let size = line.utf8.count
                lines += 1
                bytes += size
                guard lines <= 400, size <= lineLimit, bytes <= byteLimit,
                      !line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return false }
            }
        }
        return lines > 0
    }
}

private struct Reply: Encodable { let groups: [[String]] }
private enum HostFailure: Error { case invalidInput, resource }

@MainActor
private final class Host: NSObject, WKURLSchemeHandler, WKNavigationDelegate {
    private static let scheme = "auvra-lyrics"
    private static let page = URL(string: "auvra-lyrics://local/index.html")!
    private let root: URL
    private let job: Job
    private var webView: WKWebView?
    private var started = false
    private var finished = false

    init(root: URL, job: Job) {
        self.root = root
        self.job = job
    }

    func start() {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.setURLSchemeHandler(self, forURLScheme: Self.scheme)
        let view = WKWebView(frame: .zero, configuration: configuration)
        view.navigationDelegate = self
        webView = view
        view.load(URLRequest(url: Self.page))
    }

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction) async -> WKNavigationActionPolicy {
        !started && action.targetFrame?.isMainFrame == true && action.request.url == Self.page ? .allow : .cancel
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        guard !started, !finished else { return }
        started = true
        let input: [String: Any] = ["source": job.source, "target": job.target, "groups": job.groups]
        webView.callAsyncJavaScript("return await translateLyrics(job)", arguments: ["job": input], in: nil, in: .page) { [weak self] result in
            guard let self else { return }
            guard case .success(let value) = result, let groups = value as? [[String]],
                  self.job.accepts(groups), let bytes = try? JSONEncoder().encode(Reply(groups: groups)) else {
                self.finish()
                return
            }
            self.finish(bytes)
        }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: any Error) { finish() }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: any Error) { finish() }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { finish() }

    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        do {
            guard task.request.httpMethod == "GET", let url = task.request.url,
                  url.scheme == Self.scheme, url.host == "local", url.port == nil,
                  url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
                  url.path == "/" + url.lastPathComponent else { throw HostFailure.resource }
            let (data, type) = try resource(url.lastPathComponent)
            let response = HTTPURLResponse(url: url, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: [
                "Content-Type": type, "Content-Length": String(data.count),
                "Cache-Control": "no-store", "Access-Control-Allow-Origin": "*"
            ])!
            task.didReceive(response)
            task.didReceive(data)
            task.didFinish()
        } catch {
            task.didFailWithError(URLError(.resourceUnavailable))
        }
    }

    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {}

    private func resource(_ name: String) throws -> (Data, String) {
        if name == "index.html" { return (Data(Self.html.utf8), "text/html; charset=utf-8") }
        if name == "host.js" { return (Data(Self.script.utf8), "text/javascript; charset=utf-8") }
        let mime: String
        let limit: Int
        switch name {
        case "bergamot.js": (mime, limit) = ("text/javascript; charset=utf-8", 1024 * 1024)
        case "bergamot.wasm": (mime, limit) = ("application/wasm", 8 * 1024 * 1024)
        default:
            guard let model = job.models.first(where: { name.hasPrefix($0 + "-") }) else { throw HostFailure.resource }
            mime = "application/octet-stream"
            switch String(name.dropFirst(model.count + 1)) {
            case "model.bin": limit = 64 * 1024 * 1024
            case "lex.bin": limit = 16 * 1024 * 1024
            case "vocab.bin": limit = 4 * 1024 * 1024
            default: throw HostFailure.resource
            }
        }
        let url = root.appendingPathComponent(name)
        let properties = try url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
        guard properties.isRegularFile == true, properties.isSymbolicLink == false,
              let size = properties.fileSize, size > 0, size <= limit else { throw HostFailure.resource }
        let data = try Data(contentsOf: url, options: .mappedIfSafe)
        guard data.count == size else { throw HostFailure.resource }
        return (data, mime)
    }

    private func finish(_ result: Data? = nil) {
        guard !finished else { return }
        finished = true
        webView?.stopLoading()
        webView?.navigationDelegate = nil
        webView = nil
        // Release the current WebKit callback before terminating the one-job host.
        DispatchQueue.main.async {
            guard let result else { exit(3) }
            do { try FileHandle.standardOutput.write(contentsOf: result); exit(0) }
            catch { exit(3) }
        }
    }

    private static let html = """
        <!doctype html><meta charset="utf-8">
        <meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src auvra-lyrics: 'wasm-unsafe-eval'; connect-src auvra-lyrics:; base-uri 'none'; form-action 'none';">
        <script src="/bergamot.js"></script><script src="/host.js"></script>
        """

    private static let script = #"""
        "use strict";
        const config = [
            "beam-size: 1", "normalize: 1.0", "word-penalty: 0", "max-length-break: 128",
            "mini-batch-words: 1024", "workspace: 128", "max-length-factor: 2.0", "skip-cost: true",
            "cpu-threads: 0", "quiet: true", "quiet-translation: true",
            "gemm-precision: int8shiftAlphaAll", "alignment: soft"
        ].join("\n");
        const encoder = new TextEncoder();

        async function bytes(name) {
            const response = await fetch("/" + name);
            if (!response.ok) throw new Error();
            return new Uint8Array(await response.arrayBuffer());
        }
        function allocate(engine, data, alignment) {
            const memory = new engine.AlignedMemory(data.length, alignment);
            memory.getByteArrayView().set(data);
            return memory;
        }
        async function loadModel(engine, name, source, target) {
            const vocabulary = new engine.AlignedMemoryList();
            let model, lex, vocab;
            try {
                model = allocate(engine, await bytes(name + "-model.bin"), 256);
                lex = allocate(engine, await bytes(name + "-lex.bin"), 64);
                vocab = allocate(engine, await bytes(name + "-vocab.bin"), 64);
                vocabulary.push_back(vocab);
                return new engine.TranslationModel(source, target, config, model, lex, vocabulary, null);
            } finally {
                vocab?.delete(); lex?.delete(); model?.delete(); vocabulary.delete();
            }
        }
        function translateBatch(engine, service, first, second, texts, html) {
            const input = new engine.VectorString();
            const options = new engine.VectorResponseOptions();
            let response;
            try {
                for (const text of texts) {
                    input.push_back(text);
                    options.push_back({qualityScores: false, alignment: Boolean(second) || html, html});
                }
                response = second ? service.translateViaPivoting(first, second, input, options)
                                  : service.translate(first, input, options);
                const result = [];
                for (let index = 0; index < response.size(); index++) {
                    const item = response.get(index);
                    try { result.push(item.getTranslatedText()); }
                    finally { item.delete(); }
                }
                return result;
            } finally {
                response?.delete(); options.delete(); input.delete();
            }
        }
        const lineBreak = /[\r\n\u000B\u000C\u0085\u2028\u2029]/u;
        const lexicalText = /[\p{L}\p{N}]/u;
        const entities = {"&": "&amp;", "<": "&lt;", ">": "&gt;"};

        function contextualMarkup(lines) {
            if (lines.length < 2 || lines.some(line => !lexicalText.test(line)) ||
                lines.slice(0, -1).every(line => /[.!?。！？…]\s*$/u.test(line))) return null;
            const spans = lines.map((line, index) =>
                `<span id="L${index}">${line.replace(/[&<>]/g, character => entities[character])}</span>`);
            const markup = spans.join("");
            return encoder.encode(markup).length <= 8192 ? markup : null;
        }
        function contextualRows(text, count) {
            if (encoder.encode(text).length > 96 * 1024) return null;
            const span = /\s*<span id="L([0-3])">([^<>]*)<\/span>/y;
            const decoder = document.createElement("textarea");
            const rows = [];
            for (let index = 0; index < count; index++) {
                const match = span.exec(text);
                if (!match || Number(match[1]) !== index) return null;
                // Only character data enters the detached element, for HTML entity decoding.
                decoder.innerHTML = match[2];
                const row = decoder.textContent.trim();
                if (!lexicalText.test(row) || lineBreak.test(row) || encoder.encode(row).length > 8192) return null;
                rows.push(row);
            }
            return text.slice(span.lastIndex).trim() ? null : rows;
        }
        function translateContextual(batch, translate) {
            const rows = new Map();
            const inputs = batch.flatMap(piece => {
                const markup = contextualMarkup(piece.lines);
                return markup ? [{piece, markup}] : [];
            });
            if (!inputs.length) return rows;
            let translated;
            try { translated = translate(inputs.map(input => input.markup), true); }
            catch { return rows; }
            if (translated.length !== inputs.length) return rows;
            for (const [index, {piece}] of inputs.entries()) {
                const aligned = contextualRows(translated[index], piece.lines.length);
                if (aligned) rows.set(piece, aligned);
            }
            return rows;
        }
        function translateGroups(job, translate) {
            const result = job.groups.map(group => Array(group.length));
            const pieces = [];
            for (const [group, lines] of job.groups.entries()) {
                for (let index = 0; index < lines.length;) {
                    const piece = lines.slice(index, index + 4);
                    if (piece.some(line => lineBreak.test(line)) ||
                        encoder.encode(piece.join("\n")).length > 4096) piece.length = 1;
                    pieces.push({group, index, lines: piece});
                    index += piece.length;
                }
            }
            let outputBytes = 0;
            for (let start = 0; start < pieces.length; start += 4) {
                const batch = pieces.slice(start, start + 4);
                const rows = job.source === "ja" ? translateContextual(batch, translate) : new Map();
                // A valid sentence can reorder spans. Preserve the original timed rows in that case.
                const pending = batch.filter(piece => !rows.has(piece));
                const translated = pending.length ? translate(pending.map(piece => piece.lines.join("\n")), false) : [];
                for (const [offset, piece] of pending.entries()) {
                    const text = translated.length === pending.length ? translated[offset] : "";
                    let lines = piece.lines.length === 1 ? [text] : text.split(/\r\n|\r|\n/);
                    const valid = values => values.length === piece.lines.length && values.every(value => value.trim());
                    if (!valid(lines)) lines = translate(piece.lines, false);
                    if (!valid(lines)) throw new Error();
                    rows.set(piece, lines);
                }
                for (const piece of batch) {
                    for (const [line, value] of rows.get(piece).entries()) {
                        const size = encoder.encode(value).length;
                        outputBytes += size;
                        if (size > 8192 || outputBytes > 96 * 1024) throw new Error();
                        result[piece.group][piece.index + line] = value;
                    }
                }
            }
            return result;
        }
        async function translateLyrics(job) {
            const wasmBinary = await bytes("bergamot.wasm");
            let engine, first, second, service;
            try {
                await new Promise((resolve, reject) => {
                    engine = loadBergamot({
                        wasmBinary, print: () => {}, printErr: () => {},
                        onRuntimeInitialized: resolve, onAbort: () => reject(new Error())
                    });
                });
                first = job.source === "ja" ? await loadModel(engine, "jaen", "ja", "en")
                                            : await loadModel(engine, "enru", "en", "ru");
                if (job.source === "ja" && job.target === "ru") {
                    second = await loadModel(engine, "enru", "en", "ru");
                }
                service = new engine.BlockingService({cacheSize: 0});
                return translateGroups(job, (texts, html) => translateBatch(engine, service, first, second, texts, html));
            } finally {
                service?.delete(); second?.delete(); first?.delete();
            }
        }
        """#
}

@main
private struct AuvraLyricsHost {
    @MainActor static func main() {
        // Covers stdin, initialization and inference, even if WebContent stops responding.
        signal(SIGALRM) { _ in _exit(4) }
        alarm(30)
        signal(SIGPIPE, SIG_IGN)
        let job: Job
        do { job = try Job.read() }
        catch { exit(2) }
        let executable = URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL
        let helpers = executable.deletingLastPathComponent()
        let contents = helpers.deletingLastPathComponent()
        guard helpers.lastPathComponent == "Helpers", contents.lastPathComponent == "Contents" else { exit(3) }
        let root = contents.resolvingSymlinksInPath().appendingPathComponent("Resources/OfflineLyrics", isDirectory: true)
        guard root.resolvingSymlinksInPath().path == root.path else { exit(3) }
        NSApplication.shared.setActivationPolicy(.prohibited)
        let host = Host(root: root, job: job)
        host.start()
        withExtendedLifetime(host) { RunLoop.main.run() }
        exit(3)
    }
}
