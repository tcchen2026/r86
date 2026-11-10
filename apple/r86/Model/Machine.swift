import Foundation

enum MachineStatus: Equatable {
    case stopped, running, paused
}

/// A VM in the library: its vm.toml settings (decoded from r86_library_list)
/// plus the state of its running session.
struct Machine: Identifiable, Equatable, Decodable {
    let directory: String
    var name: String
    var os: String
    var memoryMiB: Int
    var disk: String
    var diskMiB: Int
    var iso: String?
    var cdrom: Bool
    var bootOrder: String
    var volume: Int
    var network: Bool
    var notes: String

    var status: MachineStatus = .stopped
    /// Guest instructions per second while running.
    var instructionsPerSecond: UInt64 = 0
    var displayWidth = 0
    var displayHeight = 0

    var id: String { directory }
    var isLive: Bool { status != .stopped }
    var memoryText: String { Format.size(mib: memoryMiB) }
    var diskText: String { Format.size(mib: diskMiB) }
    var isoName: String? { iso.map { URL(filePath: $0).lastPathComponent } }

    var statusText: String {
        switch status {
        case .running: return "Running"
        case .paused: return "Paused"
        case .stopped: return "Off"
        }
    }

    var specRows: [(String, String)] {
        [
            ("System", os),
            ("Memory", memoryText),
            ("Disk", "\(diskText) · \(URL(filePath: disk).lastPathComponent)"),
            ("CD", cdrom ? (isoName ?? "Empty") : "None"),
            ("Boot", bootOrder == "cdrom" ? "CD first" : "Hard disk first"),
            ("Network", network ? "User (NAT)" : "None"),
            ("Sound", Format.volume(volume)),
        ]
    }

    enum CodingKeys: String, CodingKey {
        case directory, name, os, memoryMiB, disk, diskMiB, iso, cdrom, bootOrder, volume, network, notes
    }
}

/// An OS preset from r86: default memory and disk for new machines.
struct Preset: Decodable, Identifiable, Hashable {
    let name: String
    let memoryMiB: Int
    let diskMiB: Int
    var id: String { name }
}

enum Format {
    static func size(mib: Int) -> String {
        if mib >= 1024 && mib % 1024 == 0 { return "\(mib / 1024) GiB" }
        if mib >= 1024 { return String(format: "%.1f GiB", Double(mib) / 1024) }
        return "\(mib) MiB"
    }

    static func volume(_ percent: Int) -> String {
        percent == 0 ? "Off" : "\(percent)%"
    }

    static func speed(_ instructionsPerSecond: UInt64) -> String {
        String(format: "%.1f MIPS", Double(instructionsPerSecond) / 1_000_000)
    }
}
