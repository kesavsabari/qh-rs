import SwiftUI

/// One message of yours whose turn changed files: rewinding to it puts them back as they were before.
struct Checkpoint: Identifiable {
    let turn: Int
    let prompt: String
    let files: [String]
    var id: Int { turn }
}

/// Inspector timeline: newest first, each with "Rewind to here". Asks before touching anything.
struct CheckpointList: View {
    @ObservedObject var project: Project
    @State private var confirm: Checkpoint?

    var body: some View {
        Header(text: "Checkpoints", trailing: project.checkpoints.isEmpty ? "" : "\(project.checkpoints.count)")
        if project.checkpoints.isEmpty {
            Text("Each message that changes files becomes a point you can go back to.")
                .font(Theme.sans(12.5)).foregroundStyle(Theme.faint).padding(.horizontal, 8)
        }
        ForEach(project.checkpoints.reversed()) { c in
            CheckpointRow(c: c, root: project.path, busy: project.current?.busy ?? false) { confirm = c }
        }
        Color.clear.frame(height: 0)
            .confirmationDialog("Rewind to before this message?", isPresented: Binding(get: { confirm != nil }, set: { if !$0 { confirm = nil } }),
                                presenting: confirm) { c in
                Button("Rewind files") { project.rewind(c.turn, chat: false) }
                Button("Rewind files and conversation") { project.rewind(c.turn, chat: true) }
                Button("Cancel", role: .cancel) {}
            } message: { c in
                Text("\(c.files.count) file\(c.files.count == 1 ? "" : "s") go back to how they were before “\(c.prompt.prefix(80))”. Later changes by qh are undone too. With the conversation, your message goes back into the composer.")
            }
    }
}

private struct CheckpointRow: View {
    let c: Checkpoint
    let root: String
    let busy: Bool
    let rewind: () -> Void
    @State private var over = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Circle().fill(Theme.accent).frame(width: 6, height: 6)
                Text(c.prompt.isEmpty ? "message \(c.turn + 1)" : c.prompt)
                    .font(Theme.sans(12.5)).foregroundStyle(Theme.ink).lineLimit(2)
                Spacer(minLength: 4)
                if over && !busy {
                    Button("Rewind", action: rewind).buttonStyle(.hand)
                        .font(Theme.sans(11.5, medium: true)).foregroundStyle(Theme.accent)
                        .help("Put these files back as they were before this message")
                }
            }
            Text(c.files.map { $0.hasPrefix(root + "/") ? String($0.dropFirst(root.count + 1)) : $0 }.joined(separator: ", "))
                .font(Theme.mono(10.5)).foregroundStyle(Theme.faint).lineLimit(1).truncationMode(.tail).padding(.leading, 12)
        }
        .padding(.horizontal, 8).padding(.vertical, 5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(Theme.ink.opacity(over ? 0.04 : 0)))
        .contentShape(Rectangle())
        .onHover { over = $0 }
        .contextMenu { Button("Rewind to here…", action: rewind).disabled(busy) }
    }
}
