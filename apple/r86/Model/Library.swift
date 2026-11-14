import AppKit
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
            guard let old = self[m.id] else { return m }
            m.failure = old.failure
            m.log = old.log
            if sessions[m.id] != nil {
                m.status = old.status
                m.instructionsPerSecond = old.instructionsPerSecond
                m.displayWidth = old.displayWidth
                m.displayHeight = old.displayHeight
                m.remoteURL = old.remoteURL
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
            update(id) { m in
                m.status = .running
                m.failure = nil
                m.log = nil
            }
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

    /// Start or stop the web console; the next poll shows the outcome.
    func setRemote(_ id: Machine.ID, _ on: Bool) {
        sessions[id]?.setRemote(on)
    }

    func openConsole(_ id: Machine.ID) {
        if let url = self[id]?.remoteURL { NSWorkspace.shared.open(url) }
    }

    /// Open the report of the error that ended the machine's last run.
    func showLog(_ id: Machine.ID) {
        if let log = self[id]?.log { NSWorkspace.shared.open(log) }
    }

    /// Turn every machine off now, flushing disks (used when quitting).
    func stopAll() {
        for (id, session) in sessions { finish(id, session) }
    }

    // MARK: Library edits

    /// What the New Machine wizard creates.
    struct NewMachine {
        enum Source {
            case cd(URL)
            case freeDOS
            case disk(URL)
        }

        var source: Source
        var name: String
        var preset: Preset
        var memoryMiB: Int
        var diskMiB: Int
        var network: Bool

        /// FreeDOS from the DOS kit: FreeDOS itself, or the DOS that runs
        /// Windows 3.1/9x/ME setup from its CD.
        var dosSetup: String? {
            switch source {
            case .cd: preset.dosSetup
            case .freeDOS: "freedos"
            case .disk: nil
            }
        }
    }

    /// Create a machine; planting FreeDOS runs off the main thread. Returns its id.
    func create(_ plan: NewMachine) async -> Machine.ID? {
        let picked: URL?
        switch plan.source {
        case .cd(let url), .disk(let url): picked = url
        case .freeDOS: picked = nil
        }
        let accessing = picked?.startAccessingSecurityScopedResource() ?? false
        defer { if accessing { picked?.stopAccessingSecurityScopedResource() } }
        do {
            let directory: String
            if let setup = plan.dosSetup {
                let (name, os, memory, disk, iso) = (plan.name, plan.preset.name, plan.memoryMiB,
                                                     plan.diskMiB, picked?.path)
                directory = try await Task.detached {
                    try R86.createDOS(name: name, os: os, memoryMiB: memory, diskMiB: disk,
                                      iso: iso, setup: setup)
                }.value
            } else if case .disk(let disk) = plan.source {
                directory = try R86.createExisting(name: plan.name, os: plan.preset.name,
                                                   memoryMiB: plan.memoryMiB, disk: disk.path)
            } else {
                directory = try R86.create(name: plan.name, os: plan.preset.name, memoryMiB: plan.memoryMiB,
                                           diskMiB: plan.diskMiB, iso: picked?.path)
            }
            if let picked { Bookmarks.remember(picked, in: directory) }
            if !plan.network { try R86.set(directory, "network", "false") }
            reload()
            show("Created \(plan.name)", systemImage: "checkmark")
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
            m.remoteURL = nil
        }
        // The guest may have changed the CD or volume while running.
        reload()
    }

    private func poll() {
        for (id, session) in sessions {
            // Status first: a stopping core thread posts its last messages with it.
            let status = session.status()
            let messages = session.takeMessages()
            for message in messages {
                show(message, systemImage: status.state == 3 ? "exclamationmark.triangle" : "info.circle")
            }
            switch status.state {
            case 2, 3:
                let log = session.log()
                finish(id, session)
                if status.state == 3 {
                    update(id) { m in
                        m.failure = messages.last
                        m.log = log
                    }
                } else if let m = self[id] {
                    show("\(m.name) is off", systemImage: "power")
                }
            default:
                session.refreshThumbnail()
                let size = session.image.map { ($0.width, $0.height) } ?? (0, 0)
                let remote = session.remoteURL()
                update(id) { m in
                    m.remoteURL = remote
                    m.status = status.state == 1 ? .paused : .running
                    m.instructionsPerSecond = status.instructions_per_second
                    m.displayWidth = size.0
                    m.displayHeight = size.1
                }
            }
        }
    }
}
