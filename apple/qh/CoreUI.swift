import AppKit
import SwiftUI

struct PlanItem: Equatable { let text: String; let status: String }   // pending | active | done

struct MemoryState {
    let path: String
    let text: String
    let instructions: [(path: String, text: String)]
}

/// One line of the activity log (`qh serve` "activity").
struct ActivityEntry: Identifiable {
    let id = UUID()
    let t: Date
    let kind: String
    let text: String
    let ok: Bool
    init(_ d: [String: Any]) {
        let raw = d["t"] as? Double ?? 0
        t = Date(timeIntervalSince1970: raw > 1e12 ? raw / 1000 : raw)
        kind = d["kind"] as? String ?? ""
        text = d["text"] as? String ?? ""
        ok = d["ok"] as? Bool ?? true
    }
    /// which filter it falls under
    var group: String {
        switch kind {
        case "cmd", "bash", "run", "verify": return "Commands"
        case "edit", "write", "file", "diff", "inline": return "Edits"
        case "net", "network", "fetch", "browse": return "Network"
        case "ask", "approve", "approval", "confirm", "deny", "budget": return "Approvals"
        default: return "Other"
        }
    }
}

/// An inline edit in flight in the code view: the instruction, the streamed preview, then before/after.
struct InlineEdit {
    let tab: String
    let path: String
    let start: Int
    let end: Int
    let instruction: String
    var id: Int?
    var preview = ""
    var before = ""
    var after = ""
    var done = false
    var error: String?
}

// MARK: auto-verify

/// The check qh ran after the turn: one quiet line; a failure shows the tail and offers a fix.
struct VerifyLine: View {
    let cmd: String, ok: Bool, secs: Double, tail: String
    @EnvironmentObject var ws: Workspace
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(ok ? "✓ \(cmd) · \(String(format: "%.1f", secs)) s" : "✗ \(cmd) failed · \(String(format: "%.1f", secs)) s")
                    .font(Theme.mono(12)).foregroundStyle(ok ? Theme.faint : Theme.accent)
                if !ok && !tail.isEmpty {
                    Button(open ? "hide output" : "show output") { open.toggle() }
                        .buttonStyle(.hand).font(Theme.mono(11.5)).foregroundStyle(Theme.faint)
                }
                if !ok {
                    Button {
                        if let p = ws.current, let t = p.current { p.fixVerify(t.name); t.status = "working" }
                    } label: {
                        Text("Fix it").font(Theme.sans(12)).foregroundStyle(Theme.card)
                            .padding(.horizontal, 10).frame(height: 24)
                            .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(Theme.ink))
                    }
                    .buttonStyle(.hand).help("Send the failure back to the agent")
                }
                Spacer(minLength: 0)
            }
            if open {
                ScrollView([.vertical, .horizontal]) {
                    Text(tail).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).textSelection(.enabled)
                        .fixedSize().frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 240)
                .padding(8).background(RoundedRectangle(cornerRadius: 5).fill(Theme.side))
            }
        }
    }
}

// MARK: plan

/// The live checklist above the composer while any step isn't done; in plan mode, approve it here.
struct PlanCard: View {
    @ObservedObject var tab: ChatTab
    @ObservedObject var project: Project
    @State private var folded = false
    @State private var edits = ""

    var body: some View {
        let done = tab.plan.filter { $0.status == "done" }.count
        VStack(alignment: .leading, spacing: 8) {
            Button { folded.toggle() } label: {
                HStack(spacing: 8) {
                    Text("Plan").font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                    Text("\(done)/\(tab.plan.count)").font(Theme.mono(11.5)).foregroundStyle(Theme.faint)
                    Spacer(minLength: 0)
                    Text(folded ? "▸" : "▾").font(Theme.mono(12)).foregroundStyle(Theme.faint)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.hand)
            if !folded {
                ScrollView {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(Array(tab.plan.enumerated()), id: \.offset) { _, i in
                            HStack(alignment: .firstTextBaseline, spacing: 8) {
                                Text(i.status == "done" ? "✓" : i.status == "active" ? "◐" : "○")
                                    .foregroundStyle(i.status == "active" ? Theme.accent : Theme.faint)
                                Text(i.text).strikethrough(i.status == "done", color: Theme.faint)
                                    .foregroundStyle(i.status == "done" ? Theme.faint : i.status == "active" ? Theme.ink : Theme.soft)
                            }
                            .font(Theme.mono(12))
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 180).fixedSize(horizontal: false, vertical: true)
                if tab.planReady {
                    HStack(spacing: 6) {
                        TextField("changes (optional)", text: $edits).textFieldStyle(.plain).font(Theme.sans(13))
                            .padding(.horizontal, 10).frame(height: 32)
                            .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.card))
                            .onSubmit { approve() }
                        AskButton(title: "Approve plan", fill: Theme.ink, text: Theme.card) { approve() }
                    }
                }
            }
        }
        .padding(12)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.tile))
        .frame(maxWidth: 700)
        .padding(.horizontal, 24).padding(.bottom, 8)
    }

    private func approve() {
        project.approvePlan(tab, edits: edits)
        edits = ""
    }
}

// MARK: memory

/// NOTES.md, editable; the instruction files qh loads, read-only below it.
struct MemoryView: View {
    @ObservedObject var project: Project
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    @State private var loaded = false
    @State private var saved = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Project memory").font(Theme.sans(16, medium: true)).foregroundStyle(Theme.ink)
                Spacer()
                if saved { Text("saved").font(Theme.mono(11.5)).foregroundStyle(Theme.faint) }
                Button("Save") { project.saveMemory(text); saved = true }.keyboardShortcut("s").disabled(!loaded)
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            if let m = project.memory {
                Text(m.path).font(Theme.mono(11)).foregroundStyle(Theme.faint).lineLimit(1).truncationMode(.head)
            }
            TextEditor(text: $text).font(Theme.mono(12.5)).scrollContentBackground(.hidden)
                .padding(8).background(RoundedRectangle(cornerRadius: 8).fill(Theme.side))
                .frame(minHeight: 260)
                .onChange(of: text) { _, _ in saved = false }
            if let m = project.memory, !m.instructions.isEmpty {
                Text("Instructions (read-only)").font(Theme.sans(12.5, medium: true)).foregroundStyle(Theme.soft)
                ScrollView {
                    VStack(alignment: .leading, spacing: 10) {
                        ForEach(Array(m.instructions.enumerated()), id: \.offset) { _, f in
                            VStack(alignment: .leading, spacing: 4) {
                                Text(f.path).font(Theme.mono(11.5)).foregroundStyle(Theme.soft)
                                Text(f.text).font(Theme.mono(11.5)).foregroundStyle(Theme.faint).textSelection(.enabled)
                                    .lineLimit(12)
                            }
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 180)
            }
            if project.memory == nil { Text("loading…").font(Theme.mono(12)).foregroundStyle(Theme.faint) }
        }
        .padding(20)
        .frame(width: 640, height: 600)
        .background(Theme.card)
        .onAppear { project.loadMemory() }
        .onChange(of: project.memory?.text, initial: true) { _, t in if let t, !loaded { text = t; loaded = true } }
    }
}

// MARK: activity

/// Everything the agent did in this tab, newest first, filterable by kind.
struct ActivityLogView: View {
    @ObservedObject var project: Project
    @Environment(\.dismiss) private var dismiss
    @State private var filter = "All"
    private let kinds = ["All", "Commands", "Edits", "Network", "Approvals"]

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Activity").font(Theme.sans(16, medium: true)).foregroundStyle(Theme.ink)
                Spacer()
                Button("Refresh") { project.loadActivity() }
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            HStack(spacing: 4) {
                ForEach(kinds, id: \.self) { k in
                    Button { filter = k } label: {
                        Text(k).font(Theme.sans(12.5)).foregroundStyle(filter == k ? Theme.ink : Theme.soft)
                            .padding(.horizontal, 10).frame(height: 26)
                            .background(RoundedRectangle(cornerRadius: 6).fill(filter == k ? Theme.tile : .clear))
                    }
                    .buttonStyle(.hand)
                }
            }
            let rows = project.activity.filter { filter == "All" || $0.group == filter }
            if rows.isEmpty {
                Text("Nothing yet.").font(Theme.sans(13)).foregroundStyle(Theme.faint)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 6) {
                        ForEach(rows) { r in
                            HStack(alignment: .firstTextBaseline, spacing: 10) {
                                Text(r.t, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                                    .foregroundStyle(Theme.faint).frame(width: 80, alignment: .leading)
                                Text(r.kind).foregroundStyle(Theme.soft).frame(width: 70, alignment: .leading)
                                Text(r.text).foregroundStyle(r.ok ? Theme.ink : Theme.accent).lineLimit(2).textSelection(.enabled)
                            }
                            .font(Theme.mono(12))
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
        .padding(20)
        .frame(width: 680, height: 560)
        .background(Theme.card)
        .onAppear { project.loadActivity() }
    }
}

// MARK: inline edit

/// The code view's ⌘I bar: the selected lines, an instruction, the streamed answer, then Keep or Discard.
struct InlineEditBar: View {
    @ObservedObject var project: Project
    let path: String
    @Binding var start: Int
    @Binding var end: Int
    let close: () -> Void
    @State private var instruction = ""
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let i = project.inline, i.path == path {
                HStack(spacing: 8) {
                    Text("lines \(i.start)–\(i.end)").font(Theme.mono(11.5)).foregroundStyle(Theme.faint)
                    Text(i.instruction).font(Theme.sans(12.5)).foregroundStyle(Theme.soft).lineLimit(1)
                    Spacer(minLength: 0)
                    if !i.done && i.error == nil { ProgressView().controlSize(.mini) }
                }
                if let err = i.error {
                    Text(err).font(Theme.mono(11.5)).foregroundStyle(Theme.accent)
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            if i.done {
                                ForEach(Array(lines(i.before).enumerated()), id: \.offset) { _, l in
                                    Text("- " + l).foregroundStyle(Theme.faint).strikethrough(color: Theme.faint)
                                }
                                ForEach(Array(lines(i.after).enumerated()), id: \.offset) { _, l in
                                    Text("+ " + l).foregroundStyle(Theme.ink)
                                }
                            } else {
                                Text(i.preview.isEmpty ? "…" : i.preview).foregroundStyle(Theme.soft)
                            }
                        }
                        .font(Theme.mono(11.5)).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 220).fixedSize(horizontal: false, vertical: true)
                    .padding(8).background(RoundedRectangle(cornerRadius: 6).fill(Theme.side))
                }
                HStack(spacing: 6) {
                    if i.done && i.error == nil {
                        AskButton(title: "Keep", key: "⌘⏎", fill: Theme.ink, text: Theme.card) { project.inlineKeep(); close() }
                            .keyboardShortcut(.return, modifiers: .command)
                    }
                    AskButton(title: "Discard", key: "esc", fill: .clear, text: Theme.soft) { project.inlineDiscard(); close() }
                        .keyboardShortcut(.cancelAction)
                    Spacer(minLength: 0)
                }
            } else {
                HStack(spacing: 8) {
                    Text("lines").font(Theme.mono(11.5)).foregroundStyle(Theme.faint)
                    TextField("", value: $start, format: .number).frame(width: 46)
                    Text("–").foregroundStyle(Theme.faint)
                    TextField("", value: $end, format: .number).frame(width: 46)
                    TextField("Edit these lines… (⏎)", text: $instruction).focused($focused)
                        .onSubmit { send() }
                    Button("Cancel") { close() }.keyboardShortcut(.cancelAction)
                }
                .textFieldStyle(.plain).font(Theme.mono(12))
            }
        }
        .padding(12)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(Theme.tile))
        .padding(10)
        .onAppear { focused = true }
    }

    private func send() {
        let t = instruction.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty, start >= 1 else { return }
        project.inlineEdit(path: path, start: start, end: max(start, end), instruction: t)
        instruction = ""
    }

    private func lines(_ s: String) -> [String] {
        var l = s.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        if l.last == "" { l.removeLast() }
        return l
    }
}

// MARK: cost guard

/// The approval card for a spending limit: what it costs so far against the budget.
struct BudgetAsk: View {
    let ask: Ask
    @ObservedObject var project: Project

    private func money(_ v: Any?) -> String {
        if let d = v as? Double { return String(format: "$%.2f", d) }
        if let i = v as? Int { return "$\(i)" }
        return v as? String ?? "?"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "dollarsign.circle").font(.system(size: 14)).foregroundStyle(Theme.accent)
                Text("Budget reached").font(Theme.sans(14, medium: true)).foregroundStyle(Theme.ink)
                Text("\(money(ask.args["spent"])) of \(money(ask.args["budget"]))").font(Theme.mono(12)).foregroundStyle(Theme.accent)
            }
            if let t = ask.args["text"] as? String, !t.isEmpty {
                Text(t).font(Theme.sans(13)).foregroundStyle(Theme.soft).textSelection(.enabled)
            }
            HStack(spacing: 6) {
                AskButton(title: "Keep spending", key: "⌘⏎", fill: Theme.ink, text: Theme.card) { project.answer(ask, "yes") }
                    .keyboardShortcut(.return, modifiers: .command)
                AskButton(title: "Stop here", key: "⌘⌫", fill: .clear, text: Theme.soft) { project.answer(ask, "no") }
                    .keyboardShortcut(.delete, modifiers: .command)
                Spacer(minLength: 0)
            }
        }
        .padding(14)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.tile))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).stroke(Theme.accent.opacity(0.5)))
        .frame(maxWidth: 700)
        .padding(.horizontal, 24).padding(.bottom, 10)
    }
}
