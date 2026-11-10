import SwiftUI

/// Settings of a stopped machine. Running machines are locked by r86.
struct EditMachineSheet: View {
    @Environment(Library.self) private var library
    @Environment(\.dismiss) private var dismiss

    private let original: Machine
    @State private var os: String
    @State private var memoryMiB: Int
    @State private var bootFromCD: Bool
    @State private var iso: String?
    @State private var newISO: URL?
    @State private var network: Bool
    @State private var volume: Int
    @State private var notes: String
    @State private var importing = false

    init(machine: Machine) {
        original = machine
        _os = State(initialValue: machine.os)
        _memoryMiB = State(initialValue: machine.memoryMiB)
        _bootFromCD = State(initialValue: machine.bootOrder == "cdrom")
        _iso = State(initialValue: machine.iso)
        _network = State(initialValue: machine.network)
        _volume = State(initialValue: machine.volume)
        _notes = State(initialValue: machine.notes)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("System", selection: $os) {
                        ForEach(R86.presets) { p in
                            Text(p.name).tag(p.name)
                        }
                    }
                    MemoryRow(memoryMiB: $memoryMiB)
                } footer: {
                    Text("The disk (\(original.diskText)) can't be resized here.")
                }

                Section("Drives") {
                    LabeledContent("CD image") {
                        HStack(spacing: 8) {
                            Text(iso.map { URL(filePath: $0).lastPathComponent } ?? "Empty")
                                .font(.callout.monospaced())
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .foregroundStyle(.secondary)
                            if iso != nil {
                                Button("Eject") {
                                    iso = nil
                                    newISO = nil
                                }
                            }
                            Button("Choose…") { importing = true }
                        }
                    }
                    Picker("Start from", selection: $bootFromCD) {
                        Text("Hard disk").tag(false)
                        Text("CD").tag(true)
                    }
                }

                Section("Devices") {
                    Toggle("User-mode network (NAT)", isOn: $network)
                    VolumePicker(volume: $volume)
                }

                Section("Notes") {
                    TextField("Notes", text: $notes, axis: .vertical)
                        .lineLimit(3...6)
                        .labelsHidden()
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Edit \(original.name)")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        save()
                        dismiss()
                    }
                }
            }
            .fileImporter(isPresented: $importing, allowedContentTypes: CDImage.types) { result in
                if case .success(let url) = result {
                    newISO = url
                    iso = url.path
                }
            }
        }
        .frame(width: 520, height: 560)
    }

    private func save() {
        if let newISO {
            let accessing = newISO.startAccessingSecurityScopedResource()
            library.remember(newISO, for: original.id)
            if accessing { newISO.stopAccessingSecurityScopedResource() }
        }
        var changes: [(String, String)] = []
        if os != original.os { changes.append(("os", os)) }
        if memoryMiB != original.memoryMiB { changes.append(("memory-mib", String(memoryMiB))) }
        if iso != original.iso { changes.append(("iso", iso ?? "")) }
        let bootOrder = bootFromCD ? "cdrom" : "disk"
        if bootOrder != original.bootOrder { changes.append(("boot-order", bootOrder)) }
        if network != original.network { changes.append(("network", String(network))) }
        if volume != original.volume { changes.append(("volume", String(volume))) }
        if notes != original.notes { changes.append(("notes", notes)) }
        if !changes.isEmpty { library.save(original.id, changes) }
    }
}
