import Observation
import SwiftUI

struct Toast: Identifiable, Equatable {
    let id = UUID()
    let text: String
    let systemImage: String
}

/// The machines in $HOME/r86/VMs and the sessions of those running.
/// Any number of machines run at once, each on its own r86 core thread.
@MainActor
@Observable
final class Library {
    static let shared = Library()

    private(set) var machines: [Machine] = []
    /// VM folders that could not be read, as messages.
    private(set) var problems: [String] = []
    private(set) var sessions: [Machine.ID: Session] = [:]
    var toast: Toast?

    /// Menu requests for the Library window.
    var requestNewMachine = false
    var requestEditID: Machine.ID?

    @ObservationIgnored private var poller: Task<Void, Never>?

    private init() {
        reload()
        poller = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(500))
                self?.poll()
            }
        }
    }

    subscript(_ id: Machine.ID?) -> Machine? {
        guard let id else { return nil }
        return machines.first { $0.id == id }
    }

    func session(_ id: Machine.ID) -> Session? { sessions[id] }

    func reload() {
        let (list, problems) = R86.list()
        machines = list.map { m in
            var m = m
            if let old = self[m.id], sessions[m.id] != nil {
                m.status = old.status
                m.instructionsPerSecond = old.instructionsPerSecond
                m.displayWidth = old.displayWidth
                m.displayHeight = old.displayHeight
            }
            return m
        }
        self.problems = problems
    }

    // MARK: Lifecycle

    func start(_ id: Machine.ID) {
        guard let m = self[id], sessions[id] == nil else { return }
        do {
            sessions[id] = try Session(machine: m)
            update(id) { $0.status = .running }
        } catch {
            show(error.localizedDescription, systemImage: "exclamationmark.triangle")
        }
    }

    func pause(_ id: Machine.ID) {
        sessions[id]?.send(.pause)
        update(id) { if $0.status == .running { $0.status = .paused } }
    }

    func resume(_ id: Machine.ID) {
        guard sessions[id] != nil else { return start(id) }
        sessions[id]?.send(.resume)
        update(id) { $0.status = .running }
    }

    /// Ask the guest to power off (ACPI power button); `force` turns it off now.
    func shutDown(_ id: Machine.ID, force: Bool = false) {
        guard let m = self[id], let session = sessions[id] else { return }
        if force {
            finish(id, session)
            show("\(m.name) turned off", systemImage: "power")
        } else {
            session.send(.powerButton)
            show("Asked \(m.name) to shut down", systemImage: "power")
        }
    }

    func restart(_ id: Machine.ID) {
        sessions[id]?.send(.reset)
    }

    func ctrlAltDel(_ id: Machine.ID) {
        sessions[id]?.send(.ctrlAltDel)
    }

    func setVolume(_ id: Machine.ID, _ percent: Int) {
        if let session = sessions[id] {
            session.setVolume(percent)
        } else {
            attempt { try R86.set(id, "volume", String(percent)) }
        }
        update(id) { $0.volume = percent }
    }

    func changeCD(_ id: Machine.ID, _ url: URL?) {
        guard let session = sessions[id] else { return }
        session.changeCD(url)
        update(id) { m in
            m.cdrom = true
            m.iso = url?.path
        }
    }

    /// The guest tools CD in the app's resources, if this build has it.
    static let toolsISO = Bundle.main.url(forResource: "r86_tools", withExtension: "iso")

    /// Insert the tools CD; the guest installs from R86-TOOL.EXE on it.
    func insertTools(_ id: Machine.ID) {
        guard let iso = Self.toolsISO else { return }
        changeCD(id, iso)
    }

    /// Turn every machine off now, flushing disks (used when quitting).
    func stopAll() {
        for (id, session) in sessions { finish(id, session) }
    }

    // MARK: Library edits

    /// Create a machine with an empty disk; returns its id.
    func create(name: String, preset: Preset, memoryMiB: Int, diskMiB: Int, network: Bool, iso: URL?) -> Machine.ID? {
        let accessing = iso?.startAccessingSecurityScopedResource() ?? false
        defer { if accessing { iso?.stopAccessingSecurityScopedResource() } }
        do {
            let directory = try R86.create(name: name, os: preset.name, memoryMiB: memoryMiB,
                                           diskMiB: diskMiB, iso: iso?.path)
            if let iso { Bookmarks.remember(iso, in: directory) }
            if !network { try R86.set(directory, "network", "false") }
            reload()
            show("Created \(name)", systemImage: "checkmark")
            return directory
        } catch {
            show(error.localizedDescription, systemImage: "exclamationmark.triangle")
            return nil
        }
    }

    /// Save edited settings of a stopped machine: (vm.toml key, value).
    func save(_ id: Machine.ID, _ changes: [(String, String)]) {
        attempt {
            for (key, value) in changes { try R86.set(id, key, value) }
        }
        reload()
    }

    func remember(_ url: URL, for id: Machine.ID) {
        Bookmarks.remember(url, in: id)
    }

    func delete(_ id: Machine.ID) {
        guard let m = self[id], sessions[id] == nil else { return }
        if attempt({ try R86.delete(id) }) {
            show("Deleted \(m.name)", systemImage: "trash")
        }
        reload()
    }

    // MARK: Toasts

    func show(_ text: String, systemImage: String) {
        toast = Toast(text: text, systemImage: systemImage)
    }

    // MARK: Private

    @discardableResult
    private func attempt(_ body: () throws -> Void) -> Bool {
        do {
            try body()
            return true
        } catch {
            show(error.localizedDescription, systemImage: "exclamationmark.triangle")
            return false
        }
    }

    private func update(_ id: Machine.ID, _ change: (inout Machine) -> Void) {
        guard let i = machines.firstIndex(where: { $0.id == id }) else { return }
        change(&machines[i])
    }

    private func finish(_ id: Machine.ID, _ session: Session) {
        session.close()
        sessions[id] = nil
        update(id) { m in
            m.status = .stopped
            m.instructionsPerSecond = 0
        }
        // The guest may have changed the CD or volume while running.
        reload()
    }

    private func poll() {
        for (id, session) in sessions {
            // Status first: a stopping core thread posts its last messages with it.
            let status = session.status()
            for message in session.takeMessages() {
                show(message, systemImage: status.state == 3 ? "exclamationmark.triangle" : "info.circle")
            }
            switch status.state {
            case 2, 3:
                finish(id, session)
                if status.state == 2, let m = self[id] {
                    show("\(m.name) is off", systemImage: "power")
                }
            default:
                session.refreshThumbnail()
                let size = session.image.map { ($0.width, $0.height) } ?? (0, 0)
                update(id) { m in
                    m.status = status.state == 1 ? .paused : .running
                    m.instructionsPerSecond = status.instructions_per_second
                    m.displayWidth = size.0
                    m.displayHeight = size.1
                }
            }
        }
    }
}
