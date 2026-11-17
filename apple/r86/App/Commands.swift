import SwiftUI

struct MachineIDKey: FocusedValueKey {
    typealias Value = String
}

extension FocusedValues {
    /// The machine in the front window: a machine window, or the Library's selection.
    var machineID: String? {
        get { self[MachineIDKey.self] }
        set { self[MachineIDKey.self] = newValue }
    }
}

/// r86 › About, File › New Machine, a Machine menu that acts on the front
/// window, and View options.
struct MachineCommands: Commands {
    let library: Library
    @Bindable var prefs: Preferences

    @FocusedValue(\.machineID) private var focusedID
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .appInfo) {
            Button("About r86") { AboutPanel.show() }
        }

        CommandGroup(replacing: .newItem) {
            Button("New Machine…") {
                openWindow(id: "library")
                library.requestNewMachine = true
            }
            .keyboardShortcut("n")
        }

        CommandMenu("Machine") {
            let machine = library[focusedID]
            let live = machine?.isLive ?? false

            Button("Start") {
                guard let id = focusedID else { return }
                library.start(id)
                if library.session(id) != nil { openWindow(id: "machine", value: id) }
            }
            .keyboardShortcut("r")
            .disabled(machine == nil || live)

            Button(machine?.status == .paused ? "Continue" : "Pause") {
                guard let m = machine else { return }
                if m.status == .paused { library.resume(m.id) } else { library.pause(m.id) }
            }
            .keyboardShortcut(".")
            .disabled(!live)

            Button("Shut Down") {
                if let id = focusedID { library.shutDown(id) }
            }
            .disabled(!live)

            Button("Turn Off Now") {
                if let id = focusedID { library.shutDown(id, force: true) }
            }
            .keyboardShortcut(".", modifiers: [.command, .option])
            .disabled(!live)

            Button("Restart") {
                if let id = focusedID { library.restart(id) }
            }
            .disabled(!live)

            Divider()

            Button("Send Ctrl+Alt+Del") {
                if let id = focusedID { library.ctrlAltDel(id) }
            }
            .disabled(machine?.status != .running)

            Button("Install r86 Tools…") {
                if let id = focusedID { library.insertTools(id) }
            }
            .disabled(!live || Library.toolsISO == nil)

            Divider()

            Toggle("Remote Management", isOn: Binding(get: { machine?.remoteURL != nil },
                                                     set: { on in if let id = focusedID { library.setRemote(id, on) } }))
            .disabled(!live)

            Button("Open Web Console") {
                if let id = focusedID { library.openConsole(id) }
            }
            .disabled(machine?.remoteURL == nil)

            Divider()

            Button("Edit…") {
                guard let id = focusedID else { return }
                openWindow(id: "library")
                library.requestEditID = id
            }
            .keyboardShortcut("i")
            .disabled(machine == nil || live)

            Button("Show Window") {
                if let id = focusedID { openWindow(id: "machine", value: id) }
            }
            .disabled(!live)
        }

        CommandGroup(after: .toolbar) {
            Picker("Scaling", selection: $prefs.scaling) {
                ForEach(DisplayScaling.allCases) { s in
                    Text(s.title).tag(s)
                }
            }
            .pickerStyle(.inline)
            Toggle("Smooth Scaling", isOn: $prefs.smoothScaling)
            Toggle("Speed Overlay", isOn: $prefs.showSpeed)
            Divider()
        }
    }
}
