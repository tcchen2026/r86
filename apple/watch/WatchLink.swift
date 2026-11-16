import Foundation
import Observation
import WatchConnectivity

/// Copies turned-off machines between r86 on the iPhone and on the Apple
/// Watch (WatchConnectivity; built into both apps). One file transfer per
/// machine: a clone of its disk image, with its portable vm.toml
/// (r86_library_export) in the metadata. The receiver adopts it into a new
/// folder (r86_library_import), so nothing is overwritten. The Watch
/// publishes its machines' names in the application context; the iPhone asks
/// for one by name, as a message or, while r86 is not open on the Watch, as
/// user info delivered when it next runs. The system continues transfers
/// while the apps are suspended; an arrival is adopted when its app runs.
@MainActor
@Observable
final class WatchLink: NSObject {
    static let shared = WatchLink()

    struct Outgoing: Identifiable {
        let id: URL
        let name: String
        let progress: Progress
    }

    /// The other device has r86: on the iPhone, a paired Watch with the app;
    /// on the Watch, the iPhone app.
    private(set) var available = false
    var copyUnavailableReason: String? {
        #if targetEnvironment(simulator)
        return "Copying machines requires a real iPhone and Apple Watch."
        #else
        return available ? nil : "Pair your iPhone and Apple Watch, then open r86 on both devices."
        #endif
    }
    /// The Watch's machines as it last published them (iPhone).
    private(set) var watchMachines: [String] = []
    /// Copies on their way to the other device.
    private(set) var sending: [Outgoing] = []

    #if os(iOS)
    static let peer = "Apple Watch"
    #else
    static let peer = "iPhone"
    #endif

    private override init() {
        super.init()
    }

    /// iPad has no WatchConnectivity.
    func activate() {
        guard WCSession.isSupported() else { return }
        WCSession.default.delegate = self
        WCSession.default.activate()
    }

    /// Copy a turned-off machine to the other device.
    func send(_ machine: Machine) {
        let library = Library.shared
        guard machine.status == .stopped else {
            return library.show("Turn off \(machine.name) to copy it", systemImage: "exclamationmark.triangle")
        }
        if let reason = copyUnavailableReason {
            return library.show(reason, systemImage: "exclamationmark.triangle")
        }
        let outbox = Self.outbox.appending(path: UUID().uuidString, directoryHint: .isDirectory)
        let disk = outbox.appending(path: URL(filePath: machine.disk).lastPathComponent)
        let (directory, name) = (machine.directory, machine.name)
        Task {
            do {
                let config = try await Task.detached {
                    try FileManager.default.createDirectory(at: outbox, withIntermediateDirectories: true)
                    return try R86.export(directory, disk: disk.path)
                }.value
                WCSession.default.transferFile(disk, metadata: ["name": name, "config": config])
                refresh()
                library.show("Copying \(name) to \(Self.peer)", systemImage: "arrow.up.circle")
            } catch {
                try? FileManager.default.removeItem(at: outbox)
                library.show(error.localizedDescription, systemImage: "exclamationmark.triangle")
            }
        }
    }

    #if os(iOS)
    /// Ask the Watch for a copy of its machine `name`.
    func request(_ name: String) {
        let session = WCSession.default
        if session.isReachable {
            session.sendMessage(["send": name], replyHandler: nil) { @Sendable _ in
                WCSession.default.transferUserInfo(["send": name])
            }
            Library.shared.show("Asked \(Self.peer) for \(name)", systemImage: "arrow.down.circle")
        } else {
            session.transferUserInfo(["send": name])
            Library.shared.show("Open r86 on your Apple Watch to send \(name)", systemImage: "applewatch")
        }
    }
    #else
    /// Tell the iPhone which machines the Watch has.
    func publish(_ names: [String]) {
        guard WCSession.isSupported(), WCSession.default.activationState == .activated else { return }
        try? WCSession.default.updateApplicationContext(["machines": names])
    }
    #endif

    // MARK: Private

    nonisolated private static let outbox = URL.temporaryDirectory.appending(path: "Outbox", directoryHint: .isDirectory)
    nonisolated private static let inbox = URL.temporaryDirectory.appending(path: "Inbox", directoryHint: .isDirectory)

    private func refresh(finished: URL? = nil) {
        let session = WCSession.default
        #if os(iOS)
        available = session.activationState == .activated && session.isPaired && session.isWatchAppInstalled
        watchMachines = session.receivedApplicationContext["machines"] as? [String] ?? []
        #else
        available = session.activationState == .activated && session.isCompanionAppInstalled
        #endif
        sending = session.outstandingFileTransfers.compactMap { transfer -> Outgoing? in
            let url = transfer.file.fileURL
            guard url != finished else { return nil }
            let name = transfer.file.metadata?["name"] as? String ?? url.lastPathComponent
            return Outgoing(id: url, name: name, progress: transfer.progress)
        }
    }

    private func adopt(config: String, disk: URL, folder: URL) {
        Task {
            let result = await Task.detached { () -> Result<String, any Error> in
                Result { try R86.adopt(config: config, disk: disk.path) }
            }.value
            try? FileManager.default.removeItem(at: folder)
            let library = Library.shared
            library.reload()
            switch result {
            case .success(let directory):
                let name = URL(filePath: directory).lastPathComponent
                library.show("Received \(name) from \(Self.peer)", systemImage: "arrow.down.circle")
            case .failure(let error):
                library.show(error.localizedDescription, systemImage: "exclamationmark.triangle")
            }
        }
    }

    /// The iPhone asked for a machine by name.
    private func sendRequested(_ name: String) {
        guard let machine = Library.shared.machines.first(where: { $0.name == name }) else {
            return Library.shared.show("\(Self.peer) asked for \(name), which is not here",
                                       systemImage: "questionmark.circle")
        }
        send(machine)
    }
}

extension WatchLink: WCSessionDelegate {
    nonisolated func session(_ session: WCSession, activationDidCompleteWith activationState: WCSessionActivationState,
                             error: (any Error)?) {
        Task { @MainActor in
            self.refresh()
            #if os(watchOS)
            self.publish(Library.shared.machines.map(\.name))
            #endif
        }
    }

    #if os(iOS)
    nonisolated func sessionDidBecomeInactive(_ session: WCSession) {}

    /// The user switched to another Watch: activate for it.
    nonisolated func sessionDidDeactivate(_ session: WCSession) {
        session.activate()
    }

    nonisolated func sessionWatchStateDidChange(_ session: WCSession) {
        Task { @MainActor in self.refresh() }
    }
    #else
    nonisolated func sessionCompanionAppInstalledDidChange(_ session: WCSession) {
        Task { @MainActor in self.refresh() }
    }
    #endif

    nonisolated func session(_ session: WCSession, didReceiveApplicationContext applicationContext: [String: Any]) {
        Task { @MainActor in self.refresh() }
    }

    /// The file is deleted when this returns: move it out first.
    nonisolated func session(_ session: WCSession, didReceive file: WCSessionFile) {
        guard let config = file.metadata?["config"] as? String else { return }
        let folder = Self.inbox.appending(path: UUID().uuidString, directoryHint: .isDirectory)
        let disk = folder.appending(path: file.fileURL.lastPathComponent)
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try FileManager.default.moveItem(at: file.fileURL, to: disk)
        } catch {
            try? FileManager.default.removeItem(at: folder)
            let message = error.localizedDescription
            Task { @MainActor in Library.shared.show(message, systemImage: "exclamationmark.triangle") }
            return
        }
        Task { @MainActor in self.adopt(config: config, disk: disk, folder: folder) }
    }

    nonisolated func session(_ session: WCSession, didFinish fileTransfer: WCSessionFileTransfer, error: (any Error)?) {
        let url = fileTransfer.file.fileURL
        let name = fileTransfer.file.metadata?["name"] as? String ?? url.lastPathComponent
        let folder = url.deletingLastPathComponent()
        if folder.deletingLastPathComponent().standardizedFileURL == Self.outbox.standardizedFileURL {
            try? FileManager.default.removeItem(at: folder)
        }
        let failure = error?.localizedDescription
        Task { @MainActor in
            self.refresh(finished: url)
            if let failure {
                Library.shared.show("\(name) was not copied: \(failure)", systemImage: "exclamationmark.triangle")
            } else {
                Library.shared.show("Copied \(name) to \(Self.peer)", systemImage: "checkmark")
            }
        }
    }

    nonisolated func session(_ session: WCSession, didReceiveMessage message: [String: Any]) {
        guard let name = message["send"] as? String else { return }
        Task { @MainActor in self.sendRequested(name) }
    }

    nonisolated func session(_ session: WCSession, didReceiveUserInfo userInfo: [String: Any]) {
        guard let name = userInfo["send"] as? String else { return }
        Task { @MainActor in self.sendRequested(name) }
    }
}
