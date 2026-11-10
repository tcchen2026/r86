import AppKit
import SwiftUI

/// r86 for the Mac: a Library window plus one window per running machine.
/// Any number of machines run at once.
@main
struct R86App: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var library = Library.shared
    @State private var prefs = Preferences.shared

    var body: some Scene {
        Window("r86", id: "library") {
            LibraryView()
                .environment(library)
                .environment(prefs)
        }
        .defaultSize(width: 920, height: 600)
        .commands {
            MachineCommands(library: library, prefs: prefs)
        }

        WindowGroup("Machine", id: "machine", for: String.self) { $machineID in
            if let machineID {
                MachineWindow(machineID: machineID)
                    .environment(library)
                    .environment(prefs)
            }
        }
        .defaultSize(width: 1024, height: 800)
        .windowResizability(.contentMinSize)

        Settings {
            SettingsView()
                .environment(library)
                .environment(prefs)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Running machines are turned off (disks flushed) only after asking.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        MainActor.assumeIsolated {
            let running = Library.shared.sessions.count
            guard running > 0 else { return .terminateNow }
            let alert = NSAlert()
            alert.messageText = running == 1 ? "A machine is running." : "\(running) machines are running."
            alert.informativeText = "Quitting turns them off without shutting down their guests. Unsaved work in them is lost."
            alert.addButton(withTitle: "Turn Off and Quit")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return .terminateCancel }
            Library.shared.stopAll()
            return .terminateNow
        }
    }
}
