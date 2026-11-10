import Foundation

struct R86Error: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// Library calls of the r86 C API (Bridge/r86.h).
enum R86 {
    /// $HOME/r86/VMs, the same layout as the handheld. Inside the sandbox
    /// $HOME is the app's container.
    static let library: URL = {
        let url = URL.homeDirectory.appending(path: "r86/VMs", directoryHint: .isDirectory)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }()

    /// bios.bin and vgabios-stdvga.bin ship in the app's resources.
    static var firmware: String { Bundle.main.resourcePath ?? "" }

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

    /// The preset an ISO image looks like, if any.
    static func detect(iso path: String) -> String? {
        take(r86_detect_iso(path)).flatMap { $0.isEmpty ? nil : $0 }
    }

    static func suggestedName(_ base: String) -> String {
        take(r86_library_suggest_name(library.path, base)) ?? base
    }

    /// Create a machine with a new empty disk; returns its directory.
    static func create(name: String, os: String, memoryMiB: Int, diskMiB: Int, iso: String?) throws -> String {
        try withOptionalCString(iso) { iso in
            try check(r86_library_create(library.path, name, os, UInt32(memoryMiB), UInt64(diskMiB), iso))
        }
        return library.appending(path: name, directoryHint: .isDirectory).path
    }

    static func set(_ directory: String, _ key: String, _ value: String) throws {
        try check(r86_library_set(directory, key, value))
    }

    static func delete(_ directory: String) throws {
        try check(r86_library_delete(directory))
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

/// Files the user picked outside the container (CD images), remembered as
/// security-scoped bookmarks in the VM folder's bookmarks.plist.
enum Bookmarks {
    private static func file(_ directory: String) -> URL {
        URL(filePath: directory).appending(path: "bookmarks.plist")
    }

    private static func load(_ directory: String) -> [String: Data] {
        guard let data = try? Data(contentsOf: file(directory)) else { return [:] }
        return (try? PropertyListDecoder().decode([String: Data].self, from: data)) ?? [:]
    }

    /// Remember a URL from a file panel. The caller holds its access.
    static func remember(_ url: URL, in directory: String) {
        guard let bookmark = try? url.bookmarkData(options: .withSecurityScope,
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
        guard let url = try? URL(resolvingBookmarkData: bookmark, options: .withSecurityScope,
                                 relativeTo: nil, bookmarkDataIsStale: &stale),
              url.startAccessingSecurityScopedResource() else { return nil }
        if stale { remember(url, in: directory) }
        return url
    }
}
