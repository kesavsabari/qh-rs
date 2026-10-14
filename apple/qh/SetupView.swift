import AppKit
import SwiftUI

/// Connect models without Terminal: the same sources as `qh setup` (local Ollama, ChatGPT sign-in,
/// API keys), then pick the default from everything they unlocked.
struct SetupView: View {
    @ObservedObject var project: Project
    @State private var keyFor: String?
    @State private var key = ""
    @State private var filter = ""

    var body: some View {
        let s = project.setup
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline) {
                Text("Connect your models").font(Theme.sans(24, medium: true)).foregroundStyle(Theme.ink)
                Spacer()
                if !(s?.needed ?? true) {
                    Button("done") { project.showSetup = false }.buttonStyle(.plain).font(Theme.mono(12)).foregroundStyle(Theme.soft)
                }
            }
            Text("Tick any mix: everything you connect stays available to switch to.")
                .font(Theme.sans(14)).foregroundStyle(Theme.soft).padding(.top, 6).padding(.bottom, 20)

            if let msg = project.setupMessage {
                Text(msg).font(Theme.mono(12)).foregroundStyle(project.setupError ? Color(nsColor: .systemRed) : Theme.accent)
                    .textSelection(.enabled).padding(.bottom, 12)
            }

            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(s?.sources ?? []) { src in row(src) }

                    if !project.models.isEmpty {
                        Text("default model").font(Theme.mono(12)).foregroundStyle(Theme.soft).padding(.top, 26)
                        TextField("search \(project.models.count) models", text: $filter).textFieldStyle(.plain)
                            .font(Theme.mono(13)).padding(10)
                            .background(RoundedRectangle(cornerRadius: 6).fill(Theme.side))
                        ForEach(project.models.filter { filter.isEmpty || $0.id.localizedCaseInsensitiveContains(filter) }.prefix(60)) { m in
                            Button { project.setDefault(m.id) } label: {
                                HStack {
                                    Text(m.id).foregroundStyle(Theme.ink)
                                    Text(m.from).foregroundStyle(Theme.faint)
                                    Spacer()
                                    if s?.def == m.id { Text("default").foregroundStyle(Theme.accent) }
                                }
                                .font(Theme.mono(12.5)).padding(.horizontal, 10).padding(.vertical, 7)
                                .background(RoundedRectangle(cornerRadius: 5).fill(s?.def == m.id ? Theme.hover : .clear))
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
            }
        }
        .frame(maxWidth: 640, alignment: .leading)
        .padding(.horizontal, 40).padding(.vertical, 34)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Theme.paper)
        .onAppear { project.loadSetup() }
    }

    @ViewBuilder
    private func row(_ src: SetupSource) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                Text(src.connected ? "●" : "○").foregroundStyle(src.connected ? Theme.accent : Theme.faint)
                VStack(alignment: .leading, spacing: 2) {
                    Text(src.label).font(Theme.sans(14, medium: true)).foregroundStyle(Theme.ink)
                    if let n = src.note { Text(n).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                    else if let a = src.account, src.connected { Text(a).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                }
                Spacer()
                switch src.kind {
                case "signin":
                    Button(src.connected ? "sign in again" : "sign in") { project.signInChatGPT() }
                        .disabled(project.setupBusy)
                case "key":
                    Button(src.connected ? "change key" : "add key") { keyFor = keyFor == src.id ? nil : src.id; key = "" }
                default:
                    if !src.running { Button("get Ollama") { NSWorkspace.shared.open(URL(string: "https://ollama.com/download")!) } }
                }
            }
            if keyFor == src.id {
                HStack(spacing: 8) {
                    SecureField("paste your \(src.id) API key", text: $key).textFieldStyle(.plain).font(Theme.mono(12.5))
                        .padding(8).background(RoundedRectangle(cornerRadius: 5).fill(Theme.side))
                        .onSubmit(save)
                    Button("save", action: save).disabled(key.isEmpty || project.setupBusy)
                }
                if let site = src.site, let url = URL(string: site.components(separatedBy: " ").first ?? ""), url.scheme != nil {
                    Link("get a key at \(url.host ?? site)", destination: url).font(Theme.mono(11))
                }
            }
        }
        .font(Theme.mono(12))
        .padding(.horizontal, 12).padding(.vertical, 10)
        .overlay(RoundedRectangle(cornerRadius: 6).stroke(Theme.rule))
    }

    private func save() {
        guard let id = keyFor, !key.isEmpty else { return }
        project.connectKey(id, key)
        key = ""
        keyFor = nil
    }
}

struct SetupSource: Identifiable {
    let id: String, label: String, kind: String
    let connected: Bool, running: Bool
    let note: String?, site: String?, account: String?
}

struct SetupState {
    let needed: Bool
    let def: String
    let sources: [SetupSource]

    init(_ e: [String: Any]) {
        needed = e["needed"] as? Bool ?? true
        def = e["default"] as? String ?? ""
        sources = (e["sources"] as? [[String: Any]] ?? []).map {
            SetupSource(id: $0["id"] as? String ?? "", label: $0["label"] as? String ?? "", kind: $0["kind"] as? String ?? "",
                        connected: $0["connected"] as? Bool ?? false, running: $0["running"] as? Bool ?? false,
                        note: $0["note"] as? String, site: $0["site"] as? String, account: $0["account"] as? String)
        }
    }
}

struct ModelChoice: Identifiable {
    let id: String
    let from: String
}
