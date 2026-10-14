import CoreServices
import Foundation

/// Calls back when one file changes on disk from anywhere: an editor, git, a command. Editors that
/// save by replacing the file (rename or delete, then create) are followed onto the new file.
final class FileWatch {
    private var src: DispatchSourceFileSystemObject?
    private let path: String
    private let changed: () -> Void

    init(_ path: String, changed: @escaping () -> Void) {
        self.path = path
        self.changed = changed
        open()
    }
    deinit { src?.cancel() }

    private func open() {
        let fd = Darwin.open(path, O_EVTONLY)
        guard fd >= 0 else {   // mid-replace: try again shortly
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in self?.open() }
            return
        }
        let s = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .extend, .delete, .rename], queue: .main)
        s.setEventHandler { [weak self] in
            guard let self, let s = self.src else { return }
            if !s.data.isDisjoint(with: [.delete, .rename]) {
                s.cancel(); self.src = nil
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in self?.open(); self?.changed() }
            } else {
                self.changed()
            }
        }
        s.setCancelHandler { Darwin.close(fd) }
        src = s
        s.resume()
    }
}

/// Calls back (at most about once a second) when files are added, removed or renamed anywhere
/// under a folder. Changes inside .git don't count.
final class FolderWatch {
    private var stream: FSEventStreamRef?
    private let changed: () -> Void

    init(_ root: String, changed: @escaping () -> Void) {
        self.changed = changed
        var ctx = FSEventStreamContext(version: 0, info: Unmanaged.passUnretained(self).toOpaque(), retain: nil, release: nil, copyDescription: nil)
        let cb: FSEventStreamCallback = { _, info, n, paths, flags, _ in
            guard let info else { return }
            let me = Unmanaged<FolderWatch>.fromOpaque(info).takeUnretainedValue()
            let ps = unsafeBitCast(paths, to: NSArray.self) as? [String] ?? []
            let shape = UInt32(kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemRenamed)
            for i in 0..<n where flags[i] & shape != 0 && !ps[i].contains("/.git/") {
                me.changed()
                return
            }
        }
        stream = FSEventStreamCreate(nil, cb, &ctx, [root] as CFArray, FSEventStreamEventId(kFSEventStreamEventIdSinceNow), 1.0,
                                     UInt32(kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagUseCFTypes))
        if let stream {
            FSEventStreamSetDispatchQueue(stream, .main)
            FSEventStreamStart(stream)
        }
    }
    deinit {
        if let stream { FSEventStreamStop(stream); FSEventStreamInvalidate(stream); FSEventStreamRelease(stream) }
    }
}
