import Foundation

/// Ranks project files for an @ mention: what you work on and what the conversation is about come
/// first, so a backend engineer in a big repo sees backend files without frontend ones crowding in.
enum FileRank {
    /// Space-separated words must each match the path, in order ("auth ctrl" → server/auth/controller.go).
    /// Returns nil for no match, else a score: higher is better.
    static func match(_ path: String, _ words: [Substring]) -> Double? {
        let p = path.lowercased()
        let base = p.split(separator: "/").last.map(String.init) ?? p
        var from = p.startIndex
        var score = 0.0
        for w in words {
            guard let r = p.range(of: w, range: from..<p.endIndex) ?? fuzzy(p, w, from) else { return nil }
            if base.contains(w) { score += 3 }                                   // in the file name itself
            if r.lowerBound == p.startIndex || p[p.index(before: r.lowerBound)] == "/" { score += 2 } // starts a segment
            score += 1
            from = r.upperBound
        }
        return score - Double(p.count) / 200                                    // shorter paths win ties
    }

    /// Letters in order with gaps ("ctrl" in "controller"), only as a last resort.
    private static func fuzzy(_ p: String, _ w: Substring, _ from: String.Index) -> Range<String.Index>? {
        var i = from, start: String.Index?
        for c in w {
            guard let j = p[i...].firstIndex(of: c) else { return nil }
            if start == nil { start = j }
            i = p.index(after: j)
        }
        return start.map { $0..<i }
    }

    static func rank(_ files: [String], query: String, heat: [String: Double], limit: Int = 6) -> [String] {
        let words = query.lowercased().split(separator: " ")
        if words.isEmpty {
            return heat.sorted { $0.value > $1.value }.map(\.key).prefix(limit * 3).map { $0 }
        }
        var best: [(String, Double)] = []
        for f in files {
            guard let m = match(f, words) else { continue }
            best.append((f, m + (heat[f] ?? 0)))
        }
        return best.sorted { $0.1 > $1.1 }.prefix(limit).map(\.0)
    }

    /// Git history: files you committed (strongest), then what anyone changed lately. Newer counts more.
    nonisolated static func gitHeat(_ root: String) -> [String: Double] {
        var heat: [String: Double] = [:]
        // who you are to git: user.email, or the default git falls back to when it isn't set
        let ident = Workspace.run("/usr/bin/git", ["-C", root, "var", "GIT_AUTHOR_IDENT"])
        let email = ident.split(separator: "<").dropFirst().first?.split(separator: ">").first.map(String.init) ?? ""
        func add(_ args: [String], _ weight: Double) {
            let out = Workspace.run("/usr/bin/git", ["-C", root, "log", "--name-only", "--format="] + args)
            let lines = out.split(separator: "\n")
            for (i, l) in lines.enumerated() {
                heat[String(l), default: 0] += weight * (1 - Double(i) / Double(max(lines.count, 1)) * 0.7)
            }
        }
        if !email.isEmpty { add(["--author=\(email)", "-n", "300"], 4) }
        add(["-n", "150"], 1.5)
        return heat.mapValues { min($0, 12) }                                   // one hot file can't drown the match
    }
}
