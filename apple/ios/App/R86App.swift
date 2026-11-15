import SwiftUI

/// r86 for iPhone and iPad: one machine on screen at a time. The layout
/// follows the window's shape (RootView); on an iPhone Duo folded like a
/// laptop, the upper half is the guest screen and the lower half its keyboard.
@main
struct R86App: App {
    @State private var library = Library.shared
    @State private var prefs = Preferences.shared
    @State private var stage = Stage.shared

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(library)
                .environment(prefs)
                .environment(stage)
        }
    }
}
