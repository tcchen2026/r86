import Observation
import SwiftUI

/// How touches on the guest screen point.
enum InputMode: String, CaseIterable, Identifiable {
    /// Taps land where you touch. Needs the guest's r86 tools helper; without
    /// it the full-screen layout falls back to the trackpad.
    case touch
    /// Finger motion moves the pointer, as on a laptop trackpad.
    case trackpad

    var id: String { rawValue }

    var title: String {
        switch self {
        case .touch: return "Touch"
        case .trackpad: return "Trackpad"
        }
    }

    var symbol: String {
        switch self {
        case .touch: return "hand.point.up.left"
        case .trackpad: return "rectangle.and.hand.point.up.left"
        }
    }
}

enum RootSheet: Identifiable {
    case newMachine
    case settings
    case edit(Machine.ID)

    var id: String {
        switch self {
        case .newMachine: return "new"
        case .settings: return "settings"
        case .edit(let id): return "edit-\(id)"
        }
    }
}

/// The iOS app's UI state: the machine on screen, sheets and input choices.
/// Choices that outlive the app are kept in UserDefaults, as Preferences does.
@MainActor
@Observable
final class Stage {
    static let shared = Stage()

    nonisolated private var store: UserDefaults { .standard }

    /// The machine on screen. iOS runs one machine at a time.
    var currentID: Machine.ID? {
        get { access(keyPath: \.currentID); return store.string(forKey: "currentMachine") }
        set { withMutation(keyPath: \.currentID) { store.set(newValue, forKey: "currentMachine") } }
    }

    var inputMode: InputMode {
        get { access(keyPath: \.inputMode); return InputMode(rawValue: store.string(forKey: "inputMode") ?? "") ?? .touch }
        set { withMutation(keyPath: \.inputMode) { store.set(newValue.rawValue, forKey: "inputMode") } }
    }

    /// Trackpad speed, 0.5 to 2.
    var pointerSpeed: Double {
        get { access(keyPath: \.pointerSpeed); return store.object(forKey: "pointerSpeed") == nil ? 1 : store.double(forKey: "pointerSpeed") }
        set { withMutation(keyPath: \.pointerSpeed) { store.set(newValue, forKey: "pointerSpeed") } }
    }

    /// A light tap under the finger for on-screen keys and clicks.
    var haptics: Bool {
        get { access(keyPath: \.haptics); return store.object(forKey: "haptics") == nil ? true : store.bool(forKey: "haptics") }
        set { withMutation(keyPath: \.haptics) { store.set(newValue, forKey: "haptics") } }
    }

    var showPicker = false
    /// The system keyboard is up.
    var keyboardUp = false
    var sheet: RootSheet?
    var importingCD = false
    /// The laptop deck shows its trackpad instead of the keys.
    var deckTrackpad = false
    /// A machine to show once the user agrees to turn off the running one.
    var pendingSwitch: (id: Machine.ID, start: Bool)?

    /// Key sending shared by the hardware keyboard, the key row, the laptop
    /// deck and typed text, so a modifier latched on one applies to all.
    let keys = KeyInput()

    private init() {}

    func current(_ library: Library) -> Machine? {
        library[currentID] ?? library.machines.first
    }

    /// Sheets wait for the picker to finish closing.
    func present(_ s: RootSheet) {
        keyboardUp = false
        if showPicker {
            showPicker = false
            Task {
                try? await Task.sleep(for: .milliseconds(450))
                self.sheet = s
            }
        } else {
            sheet = s
        }
    }

    /// Show a machine, starting it if asked. Another running machine must be
    /// turned off first; r86 cannot save a running machine, so ask.
    func select(_ id: Machine.ID, library: Library, start: Bool = false) {
        if library.sessions.keys.contains(where: { $0 != id }) {
            pendingSwitch = (id, start)
            return
        }
        show(id, library: library, start: start)
    }

    /// The user agreed: turn the other machines off, then switch.
    func confirmSwitch(_ library: Library) {
        guard let pending = pendingSwitch else { return }
        pendingSwitch = nil
        for other in library.sessions.keys where other != pending.id {
            library.shutDown(other, force: true)
        }
        show(pending.id, library: library, start: pending.start)
    }

    private func show(_ id: Machine.ID, library: Library, start: Bool) {
        keys.releaseAll()
        deckTrackpad = false
        currentID = id
        if start { library.start(id) }
    }
}
