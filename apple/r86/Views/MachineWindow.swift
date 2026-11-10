import SwiftUI
import UniformTypeIdentifiers

/// One window per machine. Closing it leaves the machine running; reopen it
/// from the Library or the Machine menu.
struct MachineWindow: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs

    let machineID: Machine.ID

    @State private var confirmStop = false
    @State private var choosingCD = false

    var body: some View {
        Group {
            if let machine = library[machineID] {
                session(machine)
            } else {
                ContentUnavailableView("Machine Deleted", systemImage: "trash",
                                       description: Text("You can close this window."))
            }
        }
        .frame(minWidth: 480, minHeight: 320)
    }

    @ViewBuilder
    private func session(_ m: Machine) -> some View {
        ZStack {
            Color.black
            if let session = library.session(m.id) {
                GuestScreen(session: session, scaling: prefs.scaling, smooth: prefs.smoothScaling,
                            sendCommandKeys: prefs.sendCommandKeys) { captured in
                    if captured {
                        library.show("Press ⌃⌥ to release the pointer", systemImage: "cursorarrow")
                    }
                }
                .opacity(m.status == .paused ? 0.5 : 1)
            }
            PowerOverlay(machine: m)
        }
        .overlay(alignment: .topTrailing) {
            if prefs.showSpeed, m.status == .running {
                SpeedOverlay(machine: m).padding(8)
            }
        }
        .overlay(alignment: .top) { ToastOverlay() }
        .navigationTitle(m.name)
        .navigationSubtitle("\(m.os) · \(m.memoryText) · \(m.statusText)")
        .toolbar { windowToolbar(m) }
        .focusedSceneValue(\.machineID, m.id)
        .confirmationDialog("Shut down \(m.name)?", isPresented: $confirmStop, titleVisibility: .visible) {
            Button("Shut Down") { library.shutDown(m.id) }
            Button("Turn Off Now", role: .destructive) { library.shutDown(m.id, force: true) }
        } message: {
            Text("Shut Down presses the power button so the guest can power off. Turn Off Now is like pulling the plug.")
        }
        .fileImporter(isPresented: $choosingCD, allowedContentTypes: CDImage.types) { result in
            if case .success(let url) = result { library.changeCD(m.id, url) }
        }
    }

    @ToolbarContentBuilder
    private func windowToolbar(_ m: Machine) -> some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            if m.status == .running {
                Button("Pause", systemImage: "pause.fill") { library.pause(m.id) }
            } else {
                Button(m.status == .paused ? "Continue" : "Start", systemImage: "play.fill") {
                    library.resume(m.id)
                }
            }
            Button("Shut Down", systemImage: "power") {
                if prefs.confirmStop { confirmStop = true } else { library.shutDown(m.id) }
            }
            .disabled(!m.isLive)
            Button("Restart", systemImage: "arrow.clockwise") { library.restart(m.id) }
                .disabled(!m.isLive)
        }
        ToolbarItemGroup(placement: .primaryAction) {
            Menu("CD", systemImage: "opticaldisc") {
                Button("Insert CD Image…") { choosingCD = true }
                Button("Eject") { library.changeCD(m.id, nil) }
                    .disabled(m.iso == nil)
            }
            .disabled(!m.isLive)
            Menu("Sound", systemImage: m.volume == 0 ? "speaker.slash" : "speaker.wave.2") {
                Picker("Sound", selection: Binding(get: { m.volume }, set: { library.setVolume(m.id, $0) })) {
                    ForEach([100, 75, 50, 25, 0], id: \.self) { level in
                        Text(Format.volume(level)).tag(level)
                    }
                }
                .pickerStyle(.inline)
            }
            Button("Ctrl+Alt+Del", systemImage: "command") { library.ctrlAltDel(m.id) }
                .help("Send Ctrl+Alt+Del")
                .disabled(m.status != .running)
        }
    }
}

enum CDImage {
    static let types: [UTType] = [UTType(filenameExtension: "iso") ?? .diskImage, .diskImage]
}
