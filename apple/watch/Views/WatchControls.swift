import SwiftUI

/// Typing and power for the machine on screen. Typing uses the system's
/// keyboard, Scribble or dictation, sent as US-layout keys.
struct WatchControls: View {
    @Environment(Library.self) private var library
    @Environment(\.dismiss) private var dismiss
    let id: Machine.ID

    @State private var text = ""
    @State private var confirmingOff = false

    var body: some View {
        List {
            if let machine = library[id], machine.isLive {
                Section {
                    TextField("Type", text: $text)
                        .onSubmit {
                            type(text)
                            text = ""
                        }
                    HStack {
                        key("Return", HID.enter)
                        key("Esc", HID.escape)
                    }
                    HStack {
                        key("Tab", HID.tab)
                        key("Delete", HID.backspace)
                    }
                }
                Section {
                    Button("Ctrl+Alt+Del", systemImage: "command") { library.ctrlAltDel(id) }
                    if machine.status == .paused {
                        Button("Continue", systemImage: "play") { library.resume(id) }
                    } else {
                        Button("Pause", systemImage: "pause") { library.pause(id) }
                    }
                    Button("Restart", systemImage: "arrow.clockwise") { library.restart(id) }
                    Button("Shut Down", systemImage: "power") {
                        library.shutDown(id)
                        dismiss()
                    }
                    Button("Turn Off Now", systemImage: "bolt.slash", role: .destructive) { confirmingOff = true }
                } footer: {
                    Text(Format.speed(machine.instructionsPerSecond))
                }
            } else {
                Text("The machine is off.")
            }
        }
        .confirmationDialog("Turn off now?", isPresented: $confirmingOff, titleVisibility: .visible) {
            Button("Turn Off", role: .destructive) {
                library.shutDown(id, force: true)
                dismiss()
            }
        } message: {
            Text("The guest does not shut down first; anything it has not saved is lost.")
        }
    }

    private func key(_ title: String, _ usage: UInt16) -> some View {
        Button(title) {
            guard let session = library.session(id) else { return }
            session.hidKey(usage, down: true)
            session.hidKey(usage, down: false)
        }
        .buttonStyle(.bordered)
    }

    private func type(_ text: String) {
        guard let session = library.session(id) else { return }
        for character in text {
            guard let key = HID.key(for: character) else { continue }
            if key.shift { session.hidKey(HID.leftShift, down: true) }
            session.hidKey(key.usage, down: true)
            session.hidKey(key.usage, down: false)
            if key.shift { session.hidKey(HID.leftShift, down: false) }
        }
    }
}
