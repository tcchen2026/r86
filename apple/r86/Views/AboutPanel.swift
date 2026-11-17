import AppKit

/// r86 › About r86: the standard About panel (icon, version, the
/// NSHumanReadableCopyright line) with credits linking the project page and
/// the licences ports/licenses.sh bundles in Resources/licenses.
enum AboutPanel {
    static let repository = URL(string: "https://github.com/tcchen2026/r86")!

    @MainActor static func show() {
        let credits = NSMutableAttributedString()
        func add(_ text: String, link: URL? = nil) {
            var attributes: [NSAttributedString.Key: Any] = [
                .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
                .foregroundColor: NSColor.labelColor,
            ]
            if let link { attributes[.link] = link }
            credits.append(NSAttributedString(string: text, attributes: attributes))
        }
        add("A heavily optimized x86 emulator: a complete PC for DOS, Linux and Windows.\n\n")
        add("github.com/tcchen2026/r86", link: repository)
        if let licenses = Bundle.main.resourceURL?.appending(path: "licenses", directoryHint: .isDirectory) {
            add("\n")
            add("Licenses and third-party notices", link: licenses)
        }
        let centered = NSMutableParagraphStyle()
        centered.alignment = .center
        credits.addAttribute(.paragraphStyle, value: centered,
                             range: NSRange(location: 0, length: credits.length))
        var options: [NSApplication.AboutPanelOptionKey: Any] = [.credits: credits]
        // In parentheses after "Version 2026.11.18": the full build version,
        // v2026.11.18-b0dd54 (script/build_macos.sh writes R86Version).
        if let version = Bundle.main.object(forInfoDictionaryKey: "R86Version") as? String {
            options[.version] = version
        }
        NSApp.orderFrontStandardAboutPanel(options: options)
    }
}
