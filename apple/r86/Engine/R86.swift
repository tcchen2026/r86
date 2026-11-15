import Foundation
#if os(iOS)
import UIKit
#endif

struct R86Error: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// Library calls of the r86 C API (Bridge/r86.h).
enum R86 {
    /// $HOME/r86/VMs, the same layout as the handheld. Inside the sandbox
    /// $HOME is the app's container. On iOS and watchOS, Documents/VMs; the
    /// Files app shows the iPhone's.
    static let library: URL = {
        let url = home.appending(path: "VMs", directoryHint: .isDirectory)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }()

    #if os(macOS)
    private static let home = URL.homeDirectory.appending(path: "r86", directoryHint: .isDirectory)
    #else
    private static let home = URL.documentsDirectory
    #endif

    /// bios.bin, vgabios-stdvga.bin and the DOS kit dos.zip ship in the
    /// app's resources.
    static var firmware: String { Bundle.main.resourcePath ?? "" }

    /// $HOME/r86/logs (iOS: Documents/logs): a VM's fatal error report,
    /// <VM folder>.log, written only when its run fails.
    static let logs = home.appending(path: "logs", directoryHint: .isDirectory)

    /// The guest mode a fresh r86 tools install sets, at 16 bpp (magic
    /// command 5): on iPhone 936x666, half the iPhone Duo's laptop half
    /// (VBE 1A1h); on the Watch 640x480; on iPad and the Mac 1024x768.
    @MainActor
    static var recommendedMode: (width: UInt16, height: UInt16) {
        #if os(iOS)
        UIDevice.current.userInterfaceIdiom == .phone ? (936, 666) : (1024, 768)
        #elseif os(watchOS)
        (640, 480)
        #else
        (1024, 768)
        #endif
    }

    /// Take ownership of a string returned by the library.
    static func take(_ s: UnsafeMutablePointer<CChar>?) -> String? {
        guard let s else { return nil }
        defer { r86_string_free(s) }
        return String(cString: s)
    }

    static func check(_ s: UnsafeMutablePointer<CChar>?) throws {
        if let message = take(s) { throw R86Error(message: message) }
    }

    static func list() -> (machines: [Machine], problems: [String]) {
        let json = take(r86_library_list(library.path)) ?? "[]"
        let entries = (try? JSONDecoder().decode([Entry].self, from: Data(json.utf8))) ?? []
        var machines: [Machine] = []
        var problems: [String] = []
        for entry in entries {
            switch entry {
            case .machine(let m): machines.append(m)
            case .problem(let p): problems.append(p)
            }
        }
        return (machines, problems)
    }

    static let presets: [Preset] = {
        let json = take(r86_presets()) ?? "[]"
        return (try? JSONDecoder().decode([Preset].self, from: Data(json.utf8))) ?? []
    }()

    /// Memory sizes the wizard and settings offer, in MiB.
    static let memoryChoices: [Int] = {
        let json = take(r86_memory_choices()) ?? "[]"
        return (try? JSONDecoder().decode([Int].self, from: Data(json.utf8))) ?? []
    }()

    /// The preset an ISO image looks like, if any.
    static func detect(iso path: String) -> String? {
        take(r86_detect_iso(path)).flatMap { $0.isEmpty ? nil : $0 }
    }

    /// The preset a disk image looks like, if any.
    static func detect(disk path: String) -> String? {
        take(r86_detect_disk(path)).flatMap { $0.isEmpty ? nil : $0 }
    }

    static func suggestedName(_ base: String) -> String {
        take(r86_library_suggest_name(library.path, base)) ?? base
    }

    /// Create a machine with a new empty disk; returns its directory.
    static func create(name: String, os: String, memoryMiB: Int, diskMiB: Int, iso: String?) throws -> String {
        try withOptionalCString(iso) { iso in
            try check(r86_library_create(library.path, name, os, UInt32(memoryMiB), UInt64(diskMiB), iso))
        }
        return directory(name)
    }

    /// Create a machine whose new disk holds FreeDOS from the bundled DOS
    /// kit, set up for `setup` ("freedos", or a preset's dosSetup to run
    /// Windows setup from `iso`). Blocks for seconds; call it off the main thread.
    static func createDOS(name: String, os: String, memoryMiB: Int, diskMiB: Int,
                          iso: String?, setup: String) throws -> String {
        try withOptionalCString(iso) { iso in
            try check(r86_library_create_dos(library.path, name, os, UInt32(memoryMiB),
                                             UInt64(diskMiB), iso, firmware, setup))
        }
        return directory(name)
    }

    /// Create a machine around an existing disk image, used in place.
    static func createExisting(name: String, os: String, memoryMiB: Int, disk: String) throws -> String {
        try check(r86_library_create_existing(library.path, name, os, UInt32(memoryMiB), disk))
        return directory(name)
    }

    private static func directory(_ name: String) -> String {
        library.appending(path: name, directoryHint: .isDirectory).path
    }

    static func set(_ directory: String, _ key: String, _ value: String) throws {
        try check(r86_library_set(directory, key, value))
    }

    static func delete(_ directory: String) throws {
        try check(r86_library_delete(directory))
    }

    /// Copy a stopped machine for the other device (iPhone or Watch): its
    /// disk image to `disk` (an APFS clone), and its portable vm.toml.
    static func export(_ directory: String, disk: String) throws -> String {
        var config: UnsafeMutablePointer<CChar>?
        try check(r86_library_export(directory, disk, &config))
        return take(config) ?? ""
    }

    /// Adopt a machine from the other device into a new folder named after
    /// it; `disk` is moved in. Returns the folder.
    static func adopt(config: String, disk: String) throws -> String {
        var directory: UnsafeMutablePointer<CChar>?
        try check(r86_library_import(library.path, config, disk, &directory))
        return take(directory) ?? ""
    }

    private enum Entry: Decodable {
        case machine(Machine)
        case problem(String)

        enum Keys: String, CodingKey { case error }

        init(from decoder: Decoder) throws {
            if let machine = try? Machine(from: decoder) {
                self = .machine(machine)
            } else {
                self = .problem(try decoder.container(keyedBy: Keys.self).decode(String.self, forKey: .error))
            }
        }
    }
}

func withOptionalCString<R>(_ s: String?, _ body: (UnsafePointer<CChar>?) throws -> R) rethrows -> R {
    guard let s else { return try body(nil) }
    return try s.withCString { try body($0) }
}

/// Files the user picked outside the container (CD and disk images),
/// remembered as security-scoped bookmarks in the VM folder's bookmarks.plist.
enum Bookmarks {
    // iOS bookmarks of picked files carry their scope without the option.
    #if os(macOS)
    private static let creation: URL.BookmarkCreationOptions = .withSecurityScope
    private static let resolution: URL.BookmarkResolutionOptions = .withSecurityScope
    #else
    private static let creation: URL.BookmarkCreationOptions = []
    private static let resolution: URL.BookmarkResolutionOptions = []
    #endif

    private static func file(_ directory: String) -> URL {
        URL(filePath: directory).appending(path: "bookmarks.plist")
    }

    private static func load(_ directory: String) -> [String: Data] {
        guard let data = try? Data(contentsOf: file(directory)) else { return [:] }
        return (try? PropertyListDecoder().decode([String: Data].self, from: data)) ?? [:]
    }

    /// Remember a URL from a file panel. The caller holds its access.
    static func remember(_ url: URL, in directory: String) {
        guard let bookmark = try? url.bookmarkData(options: creation,
                                                    includingResourceValuesForKeys: nil,
                                                    relativeTo: nil) else { return }
        var all = load(directory)
        all[url.path] = bookmark
        if let data = try? PropertyListEncoder().encode(all) {
            try? data.write(to: file(directory), options: .atomic)
        }
    }

    /// Start access to a remembered path; returns the URL to stop later.
    static func access(_ path: String, in directory: String) -> URL? {
        guard let bookmark = load(directory)[path] else { return nil }
        var stale = false
        guard let url = try? URL(resolvingBookmarkData: bookmark, options: resolution,
                                 relativeTo: nil, bookmarkDataIsStale: &stale),
              url.startAccessingSecurityScopedResource() else { return nil }
        if stale { remember(url, in: directory) }
        return url
    }
}
