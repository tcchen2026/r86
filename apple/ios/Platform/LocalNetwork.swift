import Network

/// The Local Network alert, shown when the user turns on remote management.
///
/// Computers reaching the web console need no permission: accepting TCP
/// connections is not a local network operation (TN3179). Guests on the
/// user-mode network reaching devices on the LAN do need it, and asking here
/// gives the alert its context. iOS has no API for the alert; browsing for a
/// Bonjour type listed in NSBonjourServices is a local network operation.
@MainActor
enum LocalNetwork {
    private static var browser: NWBrowser?

    static func ask() {
        guard browser == nil else { return }
        let browser = NWBrowser(for: .bonjour(type: "_r86._tcp", domain: nil), using: .tcp)
        browser.start(queue: .main)
        self.browser = browser
        Task {
            // Long enough for the alert to appear; the choice is remembered.
            try? await Task.sleep(for: .seconds(10))
            browser.cancel()
            self.browser = nil
        }
    }
}
