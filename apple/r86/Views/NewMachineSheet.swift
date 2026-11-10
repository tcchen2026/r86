import SwiftUI
import UniformTypeIdentifiers

/// Three steps: System, Hardware, Summary.
struct NewMachineSheet: View {
    @Environment(Library.self) private var library
    @Environment(\.dismiss) private var dismiss

    /// Called after the machine is created: (new machine, start it now).
    var onCreated: (Machine.ID, Bool) -> Void

    @State private var step = 0
    @State private var preset = R86.presets.first(where: { $0.name == "Windows 98" }) ?? R86.presets[0]
    @State private var iso: URL?
    @State private var name = ""
    @State private var memoryMiB = 64
    @State private var diskMiB = 512
    @State private var network = true
    @State private var startAfter = true
    @State private var importing = false

    private let titles = ["System", "Hardware", "Summary"]
    private var isLast: Bool { step == titles.count - 1 }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    StepHeader(index: step, titles: titles)
                }
                switch step {
                case 0: systemStep
                case 1: hardwareStep
                default: summaryStep
                }
            }
            .formStyle(.grouped)
            .navigationTitle("New Machine")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    if step == 0 {
                        Button("Cancel") { dismiss() }
                    } else {
                        Button("Back") { withAnimation { step -= 1 } }
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(isLast ? "Create" : "Continue") { advance() }
                        .disabled(isLast && name.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .fileImporter(isPresented: $importing, allowedContentTypes: CDImage.types) { result in
                if case .success(let url) = result { choose(url) }
            }
            .onAppear { apply(preset) }
        }
        .frame(width: 520, height: 560)
    }

    private func apply(_ p: Preset) {
        memoryMiB = p.memoryMiB
        diskMiB = p.diskMiB
        name = R86.suggestedName(p.name)
    }

    /// Use a picked CD image and the system it looks like.
    private func choose(_ url: URL) {
        iso = url
        let accessing = url.startAccessingSecurityScopedResource()
        defer { if accessing { url.stopAccessingSecurityScopedResource() } }
        if let detected = R86.detect(iso: url.path),
           let match = R86.presets.first(where: { $0.name == detected }) {
            preset = match
            apply(match)
        }
    }

    private func advance() {
        guard isLast else {
            withAnimation { step += 1 }
            return
        }
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard let id = library.create(name: trimmed, preset: preset, memoryMiB: memoryMiB,
                                      diskMiB: diskMiB, network: network, iso: iso) else { return }
        dismiss()
        onCreated(id, startAfter)
    }

    // MARK: Steps

    @ViewBuilder
    private var systemStep: some View {
        Section("Operating system") {
            Picker("Operating system", selection: $preset) {
                ForEach(R86.presets) { p in
                    Text(p.name).tag(p)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
            .onChange(of: preset) { _, p in apply(p) }
        }
        Section {
            LabeledContent("CD image") {
                HStack(spacing: 8) {
                    Text(iso?.lastPathComponent ?? "None")
                        .font(.callout.monospaced())
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .foregroundStyle(.secondary)
                    if iso != nil {
                        Button("Remove") { iso = nil }
                    }
                    Button("Choose…") { importing = true }
                }
            }
        } header: {
            Text("Install from")
        } footer: {
            Text("Bring your own installation CD. A machine with a CD image starts from it.")
        }
    }

    @ViewBuilder
    private var hardwareStep: some View {
        Section {
            MemoryRow(memoryMiB: $memoryMiB)
            DiskRow(diskMiB: $diskMiB)
        } header: {
            Text("Resources")
        } footer: {
            Text("The disk is a dynamic VHD: it grows as the guest writes.")
        }
        Section("Network") {
            Toggle("User-mode network (NAT)", isOn: $network)
        }
    }

    @ViewBuilder
    private var summaryStep: some View {
        Section {
            TextField("Name", text: $name)
        }
        Section {
            LabeledContent("System", value: preset.name)
            LabeledContent("CD image", value: iso?.lastPathComponent ?? "None")
            LabeledContent("Memory", value: Format.size(mib: memoryMiB))
            LabeledContent("Disk", value: Format.size(mib: diskMiB))
            LabeledContent("Network", value: network ? "User (NAT)" : "None")
        }
        Section {
            Toggle("Start after creating", isOn: $startAfter)
        }
    }
}

struct StepHeader: View {
    let index: Int
    let titles: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 4) {
                ForEach(titles.indices, id: \.self) { i in
                    Capsule()
                        .fill(i <= index ? AnyShapeStyle(.tint) : AnyShapeStyle(.quaternary))
                        .frame(height: 4)
                }
            }
            Text("\(index + 1) of \(titles.count) · \(titles[index])")
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}
