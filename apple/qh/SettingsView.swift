import SwiftUI

/// ⌘, : qh's settings. Everything but Appearance lives in config.toml, read and written through
/// `qh __config`, so the terminal and the app always agree.
struct SettingsView: View {
    @EnvironmentObject var ws: Workspace
    @StateObject private var cfg = ConfigStore()
    @AppStorage("settingsTab") private var tab = "General"
    private let tabs: [(String, String)] = [("General", "gearshape"), ("Models", "cpu"), ("Safety", "lock.shield"),
                                            ("Team", "person.2"), ("Appearance", "paintbrush"), ("Advanced", "slider.horizontal.3")]

    // the Zen layout: sections as tabs on the ground, the open one in a floating card
    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            VStack(alignment: .leading, spacing: 2) {
                ForEach(tabs, id: \.0) { t in
                    Row(selected: tab == t.0, action: { tab = t.0 }) {
                        Label(t.0, systemImage: t.1).foregroundStyle(tab == t.0 ? Theme.ink : Theme.soft)
                    }
                }
                Spacer()
                Text(ws.version.replacingOccurrences(of: "qh ", with: "v") + " · config.toml")
                    .font(Theme.mono(10.5)).foregroundStyle(Theme.faint).padding(.horizontal, 8).padding(.bottom, 6)
            }
            .frame(width: 170)
            .padding(.top, 6)
            Group {
                switch tab {
                case "Models": Models(cfg: cfg)
                case "Safety": Safety(cfg: cfg)
                case "Team": TeamTab(cfg: cfg)
                case "Appearance": Appearance()
                case "Advanced": Advanced(cfg: cfg)
                default: General(cfg: cfg)
                }
            }
            .scrollContentBackground(.hidden)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Theme.paper, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).stroke(Theme.line, lineWidth: 0.5))
            .overlay(alignment: .bottom) {
                if let e = cfg.error {
                    Text(e).font(Theme.sans(11.5)).foregroundStyle(.red).padding(8)
                }
            }
        }
        .padding(8)
        .frame(width: 720, height: 500)
        .background(Theme.ground)
        .onAppear { cfg.qh = ws.qh; cfg.load() }
    }
}

/// config.toml as `qh __config` prints it, and one setting at a time back.
@MainActor
final class ConfigStore: ObservableObject {
    @Published var v: [String: Any] = [:]
    @Published var error: String?
    var qh: String?

    func load() {
        guard let qh else { error = "qh isn't installed"; return }
        Task.detached {
            let out = Workspace.run(qh, ["__config"])
            let map = (try? JSONSerialization.jsonObject(with: Data(out.utf8)) as? [String: Any]) ?? [:]
            await MainActor.run { self.v = map }
        }
    }

    /// `literal` is TOML: "\"x\"", "true", "3", "[\"a\"]"; empty removes the setting
    func set(_ key: String, _ literal: String) {
        guard let qh else { return }
        Task.detached {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: qh)
            p.arguments = ["__config", "set", key, literal]
            let err = Pipe()
            p.standardError = err
            p.standardOutput = Pipe()
            try? p.run()
            p.waitUntilExit()
            let msg = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            await MainActor.run {
                self.error = p.terminationStatus == 0 ? nil : (msg.isEmpty ? "couldn't save \(key)" : msg)
                self.load()
            }
        }
    }

    func str(_ k: String) -> String { v[k] as? String ?? "" }
    func bool(_ k: String) -> Bool { v[k] as? Bool ?? false }
    func num(_ k: String) -> Double { (v[k] as? NSNumber)?.doubleValue ?? 0 }
    func list(_ k: String) -> String { (v[k] as? [String] ?? []).joined(separator: ", ") }

    func setStr(_ k: String, _ s: String) { set(k, s.isEmpty ? "" : Self.quote(s)) }
    func setList(_ k: String, _ s: String) {
        let items = s.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        set(k, items.isEmpty ? "" : "[" + items.map(Self.quote).joined(separator: ", ") + "]")
    }
    static func quote(_ s: String) -> String {
        "\"" + s.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") + "\""
    }

    /// a binding that saves on change: for pickers and toggles
    func binding(_ k: String, save: @escaping (String) -> Void) -> Binding<String> {
        Binding(get: { self.str(k) }, set: { self.v[k] = $0; save($0) })
    }
    func toggle(_ k: String, save: @escaping (Bool) -> Void) -> Binding<Bool> {
        Binding(get: { self.bool(k) }, set: { self.v[k] = $0; save($0) })
    }
}

/// A text field that saves when you press Return or leave it, not on every key.
private struct Saved: View {
    let title: String
    let value: String
    var prompt = ""
    var secure = false
    let save: (String) -> Void
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        Group {
            if secure { SecureField(title, text: $text, prompt: Text(prompt)) } else { TextField(title, text: $text, prompt: Text(prompt)) }
        }
        .focused($focused)
        .onAppear { text = value }
        .onChange(of: value) { _, v in if !focused { text = v } }
        .onSubmit { if text != value { save(text) } }
        .onChange(of: focused) { _, f in if !f && text != value { save(text) } }
    }
}

private struct Note: View {
    let text: String
    init(_ t: String) { text = t }
    var body: some View { Text(text).font(Theme.sans(11.5)).foregroundStyle(Theme.soft) }
}

private struct General: View {
    @ObservedObject var cfg: ConfigStore
    @AppStorage("reopenLast") private var reopen = true
    var body: some View {
        Form {
            Picker("New tabs start in", selection: cfg.binding("mode") { cfg.setStr("mode", $0) }) {
                Text("Ask before every change").tag("ask")
                Text("Edit files, ask for commands").tag("auto-edit")
                Text("Auto (inside the sandbox)").tag("auto")
            }
            Picker("Notifications", selection: cfg.binding("notify") { cfg.setStr("notify", $0) }) {
                Text("Bell + terminal notification").tag("auto")
                Text("Desktop notifications").tag("desktop")
                Text("Off").tag("off")
            }
            Toggle("Check for updates daily", isOn: cfg.toggle("update_check") { cfg.set("update_check", $0 ? "true" : "false") })
            Toggle("Reopen the last project on launch", isOn: $reopen)
            Note("Changes apply to new tabs and the next start of qh.")
        }
        .formStyle(.grouped)
    }
}

private struct Models: View {
    @ObservedObject var cfg: ConfigStore
    @EnvironmentObject var ws: Workspace
    var body: some View {
        Form {
            Section {
                Saved(title: "Default model", value: cfg.str("model"), prompt: "provider:model") { cfg.setStr("model", $0) }
                Saved(title: "Fallback models", value: cfg.list("fallback_model"), prompt: "comma-separated, tried in order") { cfg.setList("fallback_model", $0) }
                Button("Connect a model…") { ws.current?.showSetup = true }.disabled(ws.current == nil)
            }
            Section("Routing") {
                Saved(title: "Ask model", value: cfg.str("ask_model"), prompt: "off") { cfg.setStr("routing.ask_model", $0) }
                Saved(title: "Fetch model", value: cfg.str("fetch_model"), prompt: "off") { cfg.setStr("routing.fetch_model", $0) }
                Note("Ask model answers /ask-other. Fetch model is a cheap model for read-only lookups.")
            }
            Section("Keys and spend") {
                Saved(title: "OpenRouter key", value: "", prompt: cfg.bool("openrouter_key") ? "set (type to replace)" : "not set", secure: true) { cfg.setStr("openrouter_key", $0) }
                Saved(title: "Warn when a session costs over $", value: String(format: "%g", cfg.num("cost_warn"))) { s in
                    if let d = Double(s) { cfg.set("cost_warn", String(d)) }
                }
                let p = cfg.v["providers"] as? [String] ?? []
                LabeledContent("Providers", value: p.isEmpty ? "none" : p.joined(separator: ", "))
                Note("Keys are stored in config.toml, readable only by you.")
            }
        }
        .formStyle(.grouped)
    }
}

private struct Safety: View {
    @ObservedObject var cfg: ConfigStore
    var body: some View {
        Form {
            Toggle("Sandbox commands", isOn: cfg.toggle("sandbox") { cfg.setStr("sandbox.mode", $0 ? "on" : "off") })
            Note("Off turns off both the safety guard and the OS sandbox, and new tabs fall back to asking.")
            Picker("Network", selection: cfg.binding("sandbox_network") { cfg.setStr("sandbox.network", $0) }) {
                Text("Allowlist: other hosts ask").tag("allowlist")
                Text("Open").tag("open")
                Text("Off").tag("off")
            }
            Saved(title: "Allowed hosts", value: cfg.list("sandbox_hosts"), prompt: "example.com, *.example.com") { cfg.setList("sandbox.allow_hosts", $0) }
            Saved(title: "Extra writable folders", value: cfg.list("sandbox_write"), prompt: "comma-separated paths") { cfg.setList("sandbox.writable", $0) }
        }
        .formStyle(.grouped)
    }
}

private struct TeamTab: View {
    @ObservedObject var cfg: ConfigStore
    var body: some View {
        Form {
            Saved(title: "Relay", value: cfg.str("relay"), prompt: "default") { cfg.setStr("relay", $0) }
            Note("\"default\" uses qh's relay, \"none\" turns relaying off, or host:port#secret from `qh relay`.")
            Toggle("Teammates' agents may read my working copy", isOn: cfg.toggle("team_share_files") { cfg.set("team_share_files", $0 ? "true" : "false") })
            Note("Your name on a team is your GitHub login.")
        }
        .formStyle(.grouped)
    }
}

private struct Appearance: View {
    @AppStorage("appearance") private var look = "system"
    @AppStorage("defaultLayout") private var layout = "chat"
    @AppStorage("codeSize") private var codeSize = 12.0
    var body: some View {
        Form {
            Picker("Theme", selection: $look) {
                Text("System").tag("system")
                Text("Light").tag("light")
                Text("Dark").tag("dark")
            }
            .onChange(of: look) { _, v in QhApp.apply(v) }
            Picker("Projects open in", selection: $layout) {
                Text("Chat").tag("chat")
                Text("Chat and Code").tag("split")
                Text("Code").tag("code")
            }
            Stepper("Code font size: \(Int(codeSize))", value: $codeSize, in: 9...20)
        }
        .formStyle(.grouped)
    }
}

private struct Advanced: View {
    @ObservedObject var cfg: ConfigStore
    var body: some View {
        Form {
            Section("Context") {
                Saved(title: "Local context size", value: String(Int(cfg.num("ctx")))) { s in if let n = Int(s) { cfg.set("ctx", String(n)) } }
                Saved(title: "Compact at (tokens)", value: String(Int(cfg.num("compact_at")))) { s in if let n = Int(s) { cfg.set("compact_at", String(n)) } }
                Saved(title: "Cloud context cap", value: String(Int(cfg.num("cloud_ctx"))), prompt: "0 = no cap") { s in if let n = Int(s) { cfg.set("cloud_ctx", String(n)) } }
            }
            Section("Language servers") {
                Toggle("Use language servers on PATH", isOn: Binding(get: { cfg.str("lsp") != "off" }, set: { v in cfg.v["lsp"] = v ? "auto" : "off"; cfg.setStr("lsp", v ? "auto" : "off") }))
                Saved(title: "Wait for diagnostics (ms)", value: String(Int(cfg.num("lsp_wait_ms")))) { s in if let n = Int(s) { cfg.set("lsp_wait_ms", String(n)) } }
            }
            Section("Everything else") {
                let mcp = cfg.v["mcp"] as? [String] ?? []
                LabeledContent("MCP servers", value: mcp.isEmpty ? "none" : mcp.joined(separator: ", "))
                LabeledContent("Hooks", value: "\(cfg.v["hooks"] as? Int ?? 0)")
                Button("Open config.toml") {
                    let path = cfg.str("path")
                    if !FileManager.default.fileExists(atPath: path) { FileManager.default.createFile(atPath: path, contents: Data()) }
                    NSWorkspace.shared.open(URL(fileURLWithPath: path))
                }
            }
        }
        .formStyle(.grouped)
    }
}
