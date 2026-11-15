import SwiftUI

/// r86 for Apple Watch (watchOS 26, arm64: Series 9 and later): runs
/// machines copied from r86 on the iPhone (WatchLink) with the interpreter,
/// one at a time, without sound. A running machine pauses when the app
/// leaves the screen and continues when it returns.
@main
struct R86WatchApp: App {
    @State private var library = Library.shared

    init() {
        WatchLink.shared.activate()
    }

    var body: some Scene {
        WindowGroup {
            MachineList()
                .environment(library)
                .onChange(of: library.machines.map(\.name), initial: true) { _, names in
                    WatchLink.shared.publish(names)
                }
        }
    }
}
