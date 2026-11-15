import UniformTypeIdentifiers

/// What the file pickers offer for CD and disk images.
enum CDImage {
    static let types: [UTType] = [UTType(filenameExtension: "iso") ?? .diskImage, .diskImage]
}

enum DiskImage {
    static let types: [UTType] = ["vhd", "img", "raw"].compactMap { UTType(filenameExtension: $0) } + [.diskImage]
}
