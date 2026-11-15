import SwiftUI
import UIKit

// MARK: - Control capsule

/// Power, keyboard, input mode and more, in one glass capsule.
struct SessionTools: View {
    @Environment(Stage.self) private var stage

    let machine: Machine
    var onCollapse: (() -> Void)? = nil
    var onInteract: () -> Void = {}
    /// The laptop deck has its own keys: no keyboard or input buttons.
    var compact = false

    var body: some View {
        HStack(spacing: 0) {
            PowerMenu(machine: machine)
            if !compact {
                ToolButton("keyboard", label: "Keyboard", active: stage.keyboardUp) {
                    onInteract()
                    stage.keyboardUp.toggle()
                }
                .disabled(machine.status != .running)
                ToolButton(stage.inputMode.symbol, label: "Input: \(stage.inputMode.title)") {
                    onInteract()
                    stage.inputMode = stage.inputMode == .touch ? .trackpad : .touch
                }
            }
            MoreMenu(machine: machine)
            if let onCollapse {
                ToolButton("chevron.up", label: "Hide controls", action: onCollapse)
            }
        }
        .padding(.horizontal, 4)
        .frame(height: 46)
        .glassEffect(.regular, in: .capsule)
    }
}

struct ToolButton: View {
    let symbol: String
    let label: String
    var active = false
    let action: () -> Void

    init(_ symbol: String, label: String, active: Bool = false, action: @escaping () -> Void) {
        self.symbol = symbol
        self.label = label
        self.active = active
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 18, weight: .medium))
                .frame(width: 46, height: 40)
                .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .foregroundStyle(active ? AnyShapeStyle(.tint) : AnyShapeStyle(.primary))
        .accessibilityLabel(label)
    }
}

/// Start, restart, shut down. Turning off without the guest's shutdown
/// loses what it has not written to disk.
struct PowerMenu: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    let machine: Machine
    @State private var confirming = false

    var body: some View {
        Menu {
            switch machine.status {
            case .running:
                Button("Restart", systemImage: "arrow.clockwise") { library.restart(machine.id) }
                Button("Pause", systemImage: "pause") { library.pause(machine.id) }
                Divider()
                Button("Shut Down", systemImage: "power") { library.shutDown(machine.id) }
                Button("Turn Off Now", systemImage: "bolt.slash", role: .destructive) { turnOff() }
            case .paused:
                Button("Continue", systemImage: "play") { library.resume(machine.id) }
                Button("Turn Off Now", systemImage: "bolt.slash", role: .destructive) { turnOff() }
            case .stopped:
                Button("Start", systemImage: "power") { library.start(machine.id) }
            }
        } label: {
            Image(systemName: "power")
                .font(.system(size: 18, weight: .medium))
                .frame(width: 46, height: 40)
                .contentShape(.rect)
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .accessibilityLabel("Power")
        .confirmationDialog("Turn off \(machine.name) now?", isPresented: $confirming, titleVisibility: .visible) {
            Button("Turn Off", role: .destructive) { library.shutDown(machine.id, force: true) }
        } message: {
            Text("The guest does not shut down first; anything it has not saved is lost.")
        }
    }

    private func turnOff() {
        if prefs.confirmStop {
            confirming = true
        } else {
            library.shutDown(machine.id, force: true)
        }
    }
}

/// Machine actions. Remote management serves the web console to computers
/// on the local network, with a new access code each time, while r86 is in
/// the foreground; the section title shows where to open it.
struct MoreMenu: View {
    @Environment(Library.self) private var library
    @Environment(Stage.self) private var stage
    let machine: Machine

    var body: some View {
        Menu {
            if machine.isLive {
                Button("Send Ctrl+Alt+Del", systemImage: "command") { library.ctrlAltDel(machine.id) }
                Menu("CD", systemImage: "opticaldisc") {
                    Button("Choose Image…") { stage.importingCD = true }
                    Button("Install r86 Tools") { library.insertTools(machine.id) }
                        .disabled(Library.toolsISO == nil)
                    Button("Eject") { library.changeCD(machine.id, nil) }
                        .disabled(machine.iso == nil)
                }
                Section(remoteTitle) {
                    Toggle("Remote Management", isOn: Binding(
                        get: { machine.remoteURL != nil },
                        set: { on in
                            if on { LocalNetwork.ask() }
                            library.setRemote(machine.id, on)
                        }))
                    if let url = machine.remoteURL {
                        Button("Copy Address", systemImage: "doc.on.doc") { UIPasteboard.general.url = url }
                    }
                }
            }
            Button("Edit \(machine.name)…", systemImage: "slider.horizontal.3") { stage.present(.edit(machine.id)) }
                .disabled(machine.isLive)
            if WatchLink.shared.available {
                Button("Copy to Apple Watch", systemImage: "applewatch") { WatchLink.shared.send(machine) }
                    .disabled(machine.isLive)
            }
            Button("Machines…", systemImage: "list.bullet") { stage.showPicker = true }
            Button("New Machine…", systemImage: "plus") { stage.present(.newMachine) }
            Divider()
            Button("Settings…", systemImage: "gearshape") { stage.present(.settings) }
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 18, weight: .medium))
                .frame(width: 46, height: 40)
                .contentShape(.rect)
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .accessibilityLabel("More")
    }

    /// Off: what remote management is. On: the address and access code to
    /// type on the computer; Copy Address carries both.
    private var remoteTitle: String {
        guard let url = machine.remoteURL, let host = url.host() else {
            return "Web console for a computer on this network"
        }
        let address = url.port.map { "\(host):\($0)" } ?? host
        let code = URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .queryItems?.first { $0.name == "code" }?.value
        return code.map { "Open http://\(address) · code \($0)" } ?? "Open http://\(address)"
    }
}

// MARK: - Machine switcher

/// The machine's name: tap to switch machines.
struct MachinePill: View {
    @Environment(Stage.self) private var stage
    let machine: Machine
    var small = false

    var body: some View {
        Button {
            stage.showPicker = true
        } label: {
            HStack(spacing: small ? 6 : 8) {
                StatusDot(status: machine.status)
                Text(machine.name)
                    .font(small ? .footnote.weight(.semibold) : .body.weight(.semibold))
                    .lineLimit(1)
                if !small {
                    Image(systemName: "chevron.down")
                        .font(.caption.weight(.bold))
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, small ? 12 : 14)
            .frame(height: small ? 30 : 40)
            .contentShape(.capsule)
        }
        .buttonStyle(.plain)
        .glassEffect(.regular.interactive(), in: .capsule)
        .modifier(MachinePickerPopover())
        .accessibilityLabel("Machines. Current: \(machine.name)")
    }
}

/// Popover on iPad, sheet on iPhone.
struct MachinePickerPopover: ViewModifier {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    @Environment(Stage.self) private var stage

    func body(content: Content) -> some View {
        @Bindable var stage = stage
        content.popover(isPresented: $stage.showPicker) {
            MachinePicker()
                .environment(library)
                .environment(prefs)
                .environment(stage)
                .presentationCompactAdaptation(.sheet)
        }
    }
}

struct MachinePicker: View {
    @Environment(Library.self) private var library
    @Environment(Stage.self) private var stage

    var body: some View {
        NavigationStack {
            List {
                Section {
                    ForEach(library.machines) { machine in
                        row(machine)
                    }
                } footer: {
                    Text("One machine runs at a time. Switching while one runs turns it off first.")
                }
                if !library.problems.isEmpty {
                    Section("Not readable") {
                        ForEach(library.problems, id: \.self) { Text($0).font(.footnote) }
                    }
                }
                if link.available {
                    watchSection
                }
            }
            .navigationTitle("Machines")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("New Machine", systemImage: "plus") { stage.present(.newMachine) }
                }
            }
        }
        .frame(minWidth: 340, idealWidth: 400, minHeight: 360, idealHeight: 470)
        .presentationDetents([.medium, .large])
    }

    private var link: WatchLink { .shared }

    /// Copies on their way, and the Watch's machines to copy back.
    private var watchSection: some View {
        Section {
            ForEach(link.sending) { item in
                VStack(alignment: .leading, spacing: 4) {
                    Text("Copying \(item.name) to Apple Watch")
                        .font(.footnote)
                    ProgressView(item.progress)
                }
            }
            ForEach(link.watchMachines, id: \.self) { name in
                HStack {
                    Label(name, systemImage: "applewatch")
                        .lineLimit(1)
                    Spacer(minLength: 8)
                    Button("Copy to iPhone") { link.request(name) }
                        .buttonStyle(.borderless)
                }
            }
        } header: {
            Text("Apple Watch")
        } footer: {
            Text("Copies of turned-off machines go between iPhone and Apple Watch under new names; nothing is replaced. Swipe a machine to copy it to the Watch.")
        }
    }

    private func row(_ m: Machine) -> some View {
        let isCurrent = m.id == stage.current(library)?.id
        return HStack(spacing: 12) {
            Button {
                stage.showPicker = false
                stage.select(m.id, library: library)
            } label: {
                HStack(spacing: 12) {
                    MachineThumbnail(machine: m).frame(width: 60)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(m.name)
                            .font(.body.weight(.semibold))
                            .lineLimit(1)
                        Text(verbatim: "\(m.os) · \(m.memoryText)")
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 8)
                    Text(m.statusText)
                        .font(.caption)
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                    if isCurrent {
                        Image(systemName: "checkmark")
                            .fontWeight(.semibold)
                            .foregroundStyle(.tint)
                    }
                }
                .contentShape(.rect)
            }
            .buttonStyle(.plain)

            Button {
                stage.present(.edit(m.id))
            } label: {
                Image(systemName: "info.circle").imageScale(.large)
            }
            .buttonStyle(.borderless)
            .disabled(m.isLive)
            .accessibilityLabel("Edit \(m.name)")
        }
        .listRowBackground(isCurrent ? Color.accentColor.opacity(0.14) : nil)
        .swipeActions {
            if link.available && !m.isLive {
                Button("Copy to Apple Watch", systemImage: "applewatch") { link.send(m) }
                    .tint(.blue)
            }
        }
    }
}
