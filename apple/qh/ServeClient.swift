import Foundation

/// The line to `qh serve` for one project: starts it (or finds the one already running), reads its
/// token, connects to its Unix socket, and hands every event line to `onEvent` on the main thread.
/// Views never touch this; `Project` turns the events into state.
final class ServeClient: @unchecked Sendable {
    /// The `qh serve` protocol major this app speaks (src/serve.rs PROTOCOL, docs/serve-protocol.md).
    static let protocolVersion = 1
    let path: String
    private let qh: String
    private var proc: Process?
    private var fd: Int32 = -1
    private let writeLock = NSLock()
    var onEvent: (@MainActor ([String: Any]) -> Void)?
    var onClose: (@MainActor (String) -> Void)?

    init(path: String, qh: String) {
        self.path = path
        self.qh = qh
    }

    /// Off the main thread: launch or find the server, then read until the socket closes.
    func start() {
        Thread.detachNewThread { [self] in
            do {
                let (sock, token) = try launch()
                try connect(sock)
                send(["type": "hello", "token": token, "protocol": ServeClient.protocolVersion])
                close(readLoop() ?? "qh serve stopped")
            } catch {
                close("\(error.localizedDescription)")
            }
        }
    }

    /// `qh serve` through your login shell, so its commands find what your terminal finds. Its first
    /// stdout line says where to connect ("listening" = we started it, "running" = it already was).
    private func launch() throws -> (String, String) {
        let p = Process()
        let shell = ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/zsh"
        p.executableURL = URL(fileURLWithPath: shell)
        p.arguments = ["-l", "-c", "cd \"$QH_CWD\" && exec \"$QH_BIN\" serve"]
        var env = ProcessInfo.processInfo.environment
        env["QH_BIN"] = qh
        env["QH_CWD"] = path
        env["QH_EDITOR"] = "desktop"
        p.environment = env
        // stderr to a file, not a pipe: nobody drains a pipe after startup and a full one would stall qh
        let log = FileManager.default.temporaryDirectory.appendingPathComponent("qh-serve-\(abs(path.hashValue)).log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let out = Pipe()
        p.standardOutput = out
        p.standardError = try FileHandle(forWritingTo: log)
        p.standardInput = FileHandle.nullDevice
        try p.run()
        let h = out.fileHandleForReading
        var buf = Data()
        // the login shell may print first (motd, rc files): skip to the JSON line
        while true {
            let chunk = h.availableData
            if chunk.isEmpty { break }
            buf.append(chunk)
            while let nl = buf.firstIndex(of: 0x0A) {
                let line = buf[buf.startIndex..<nl]
                buf = Data(buf[buf.index(after: nl)...])
                if let j = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                   let type = j["type"] as? String, type == "listening" || type == "running",
                   let sock = j["socket"] as? String, let tf = j["token_file"] as? String {
                    // no protocol field = a qh from before 0.2.4, which speaks 1
                    if let v = j["protocol"] as? Int, v != ServeClient.protocolVersion {
                        if type == "listening" { p.terminate() }
                        throw NSError(domain: "qh", code: 5, userInfo: [NSLocalizedDescriptionKey:
                            "qh serve speaks protocol \(v), this app speaks \(ServeClient.protocolVersion); update \(v > ServeClient.protocolVersion ? "the app" : "qh")"])
                    }
                    if type == "listening" { proc = p }
                    let token = try String(contentsOfFile: tf, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
                    return (sock, token)
                }
            }
        }
        p.waitUntilExit()
        let msg = ((try? String(contentsOf: log, encoding: .utf8)) ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        throw NSError(domain: "qh", code: 1, userInfo: [NSLocalizedDescriptionKey: msg.isEmpty ? "qh serve didn't start" : msg])
    }

    private func connect(_ sock: String) throws {
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw NSError(domain: "qh", code: 2, userInfo: [NSLocalizedDescriptionKey: "socket() failed"]) }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(sock.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            throw NSError(domain: "qh", code: 3, userInfo: [NSLocalizedDescriptionKey: "socket path too long"])
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard r == 0 else { throw NSError(domain: "qh", code: 4, userInfo: [NSLocalizedDescriptionKey: "can't connect to qh serve"]) }
    }

    /// Returns why it stopped when the server said so (a refused hello), else nil.
    private func readLoop() -> String? {
        var buf = Data()
        var chunk = [UInt8](repeating: 0, count: 65536)
        while true {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { return nil }
            buf.append(contentsOf: chunk[0..<n])
            while let nl = buf.firstIndex(of: 0x0A) {
                let line = buf[buf.startIndex..<nl]
                buf = Data(buf[buf.index(after: nl)...])
                guard let j = try? JSONSerialization.jsonObject(with: line) as? [String: Any] else { continue }
                if j["type"] as? String == "error", j["code"] as? String == "protocol" { return j["message"] as? String ?? "protocol mismatch" }
                let cb = onEvent
                DispatchQueue.main.async { MainActor.assumeIsolated { cb?(j) } }
            }
        }
    }

    func send(_ msg: [String: Any]) {
        guard fd >= 0, var d = try? JSONSerialization.data(withJSONObject: msg) else { return }
        d.append(0x0A)
        writeLock.lock()
        defer { writeLock.unlock() }
        d.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = write(fd, raw.baseAddress! + off, raw.count - off)
                if n <= 0 { return }
                off += n
            }
        }
    }

    private func close(_ why: String) {
        if fd >= 0 { Darwin.close(fd); fd = -1 }
        let cb = onClose
        DispatchQueue.main.async { MainActor.assumeIsolated { cb?(why) } }
    }

    /// Ends the server only if this app started it; one you started yourself keeps running.
    func stop() {
        if fd >= 0 { shutdown(fd, SHUT_RDWR) }
        proc?.terminate()
    }
}
