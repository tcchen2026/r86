import SwiftUI
import UniformTypeIdentifiers

/// New Machine, step by step as the handheld's wizard (handheld/screens.rs):
/// how to start (a CD image, FreeDOS from the bundled DOS kit, or an
/// existing disk), which OS, memory, disk size, then the name and Create and
/// Start. Windows 3.1/9x/ME CDs get FreeDOS planted so their setup runs from
/// DOS. The handheld's downloads are not offered.
struct NewMachineSheet: View {
    @Environment(Library.self) private var library
    @Environment(\.dismiss) private var dismiss

    /// Called after the machine is created: (new machine, start it now).
    var onCreated: (Machine.ID, Bool) -> Void

    private enum Source { case freeDOS, cd, disk }
    private enum Step { case source, os, memory, disk, summary }

    @State private var step = Step.source
    @State private var source = Source.freeDOS
    @State private var media: URL?
    @State private var detected: String?
    @State private var preset = R86.presets.first(where: { $0.name == "Other" }) ?? R86.presets[0]
    @State private var memoryMiB = 128
    @State private var diskMiB = 2048
    @State private var name = ""
    /// The name last suggested; replaced while the user has not edited it.
    @State private var suggested = ""
    @State private var network = true
    @State private var importing = false
    @State private var creating = false

    private var steps: [Step] {
        switch source {
        case .cd: [.source, .os, .memory, .disk, .summary]
        case .freeDOS: [.source, .summary]
        case .disk: [.source, .os, .memory, .summary]
        }
    }

    private var index: Int { steps.firstIndex(of: step) ?? 0 }

    private static func title(_ step: Step) -> String {
        switch step {
        case .source: "How to start?"
        case .os: "Which OS is it?"
        case .memory: "Memory"
        case .disk: "Disk size (grows as used)"
        case .summary: "Ready"
        }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    StepHeader(index: index, titles: steps.map(Self.title))
                }
                switch step {
                case .source: sourceStep
                case .os: osStep
                case .memory: memoryStep
                case .disk: diskStep
                case .summary: summaryStep
                }
            }
            .formStyle(.grouped)
            .navigationTitle("New Machine")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    if index == 0 {
                        Button("Cancel") { dismiss() }
                    } else {
                        Button("Back") { withAnimation { step = steps[index - 1] } }
                            .disabled(creating)
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if step == .summary {
                        Button("Create and Start") { create() }
                            .disabled(creating || name.trimmingCharacters(in: .whitespaces).isEmpty)
                    } else {
                        Button("Continue") { advance() }
                    }
                }
            }
            .fileImporter(isPresented: $importing,
                          allowedContentTypes: source == .cd ? CDImage.types : DiskImage.types) { result in
                if case .success(let url) = result { choose(url) }
            }
        }
        #if os(macOS)
        .frame(width: 520, height: 560)
        #endif
    }

    private func use(_ p: Preset) {
        preset = p
        memoryMiB = p.memoryMiB
        diskMiB = p.diskMiB
    }

    private func suggest(_ base: String) {
        if name.isEmpty || name == suggested {
            suggested = R86.suggestedName(base)
            name = suggested
        }
    }

    private func advance() {
        switch step {
        case .source:
            media = nil
            detected = nil
            if source == .freeDOS {
                // As the handheld's catalog entry (src/frontend/catalog.rs).
                use(R86.presets.first(where: { $0.name == "DOS" }) ?? preset)
                memoryMiB = 32
                diskMiB = 512
                suggest("FreeDOS")
                withAnimation { step = .summary }
            } else {
                importing = true
            }
        case .os:
            use(preset)
            withAnimation { step = .memory }
        case .memory:
            suggest(preset.name)
            withAnimation { step = source == .cd ? .disk : .summary }
        case .disk:
            withAnimation { step = .summary }
        case .summary:
            create()
        }
    }

    /// Use a picked CD or disk image and the system it looks like.
    private func choose(_ url: URL) {
        media = url
        let accessing = url.startAccessingSecurityScopedResource()
        defer { if accessing { url.stopAccessingSecurityScopedResource() } }
        detected = source == .cd ? R86.detect(iso: url.path) : R86.detect(disk: url.path)
        use(R86.presets.first(where: { $0.name == (detected ?? "Other") }) ?? preset)
        withAnimation { step = .os }
    }

    private func create() {
        let plan: Library.NewMachine.Source
        switch source {
        case .cd:
            guard let media else { return }
            plan = .cd(media)
        case .freeDOS:
            plan = .freeDOS
        case .disk:
            guard let media else { return }
            plan = .disk(media)
        }
        let machine = Library.NewMachine(source: plan, name: name.trimmingCharacters(in: .whitespaces),
                                         preset: preset, memoryMiB: memoryMiB, diskMiB: diskMiB,
                                         network: network)
        creating = true
        Task {
            let id = await library.create(machine)
            creating = false
            guard let id else { return }
            dismiss()
            onCreated(id, true)
        }
    }

    // MARK: Steps

    /// A choice with its detail on the right, as the handheld's lists.
    private func choice(_ title: String, _ detail: String) -> some View {
        // The picker's labelsHidden also hides nested LabeledContent labels.
        HStack {
            Text(title)
            Spacer()
            Text(detail).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var sourceStep: some View {
        Section {
            Picker("How to start?", selection: $source) {
                choice("Install FreeDOS", "included with this app").tag(Source.freeDOS)
                choice("Install from a CD image", ".iso").tag(Source.cd)
                choice("Use an existing disk", ".vhd, .img").tag(Source.disk)
            }
            .pickerStyle(.inline)
            .labelsHidden()
        } footer: {
            Text("Start with the included FreeDOS, install from a CD image, or use an existing disk.")
        }
    }

    @ViewBuilder
    private var osStep: some View {
        Section {
            Picker("Operating system", selection: $preset) {
                ForEach(R86.presets) { p in
                    choice(p.name, p.name == detected ? "detected" : "").tag(p)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
        } footer: {
            Text(detected == nil
                 ? "r86 did not recognise the image; choose the closest."
                 : "r86 recognised the image. Its choice sets the memory and disk size.")
        }
    }

    @ViewBuilder
    private var memoryStep: some View {
        Section {
            Picker("Memory", selection: $memoryMiB) {
                ForEach(R86.memoryChoices, id: \.self) { mib in
                    choice(Format.size(mib: mib), mib == preset.memoryMiB ? "recommended" : "").tag(mib)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
        }
    }

    @ViewBuilder
    private var diskStep: some View {
        Section {
            Picker("Disk size", selection: $diskMiB) {
                ForEach(preset.diskChoices, id: \.self) { mib in
                    choice(Format.size(mib: mib), mib == preset.diskMiB ? "recommended" : "").tag(mib)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
        } footer: {
            Text("The disk file starts small and grows as the guest writes, up to this size.")
        }
    }

    private var hint: String {
        switch source {
        case .cd where preset.dosSetup != nil:
            "Setup runs from DOS: r86 puts FreeDOS on the new disk, and it starts Windows Setup from the CD."
        case .cd: "The VM starts from the CD to install."
        case .freeDOS: "FreeDOS is installed on the new disk."
        case .disk: "The disk image is used where it is, not copied."
        }
    }

    @ViewBuilder
    private var summaryStep: some View {
        Section {
            TextField("Name", text: $name)
        }
        Section {
            LabeledContent("OS", value: preset.name)
            LabeledContent("Memory", value: Format.size(mib: memoryMiB))
            if source != .disk {
                LabeledContent("Disk size", value: Format.size(mib: diskMiB))
            }
            switch source {
            case .cd: LabeledContent("CD", value: media?.lastPathComponent ?? "")
            case .freeDOS: LabeledContent("FreeDOS", value: "included")
            case .disk: LabeledContent("Disk", value: media?.lastPathComponent ?? "")
            }
            Toggle("User-mode network (NAT)", isOn: $network)
        } footer: {
            Text(hint)
        }
        if creating {
            Section {
                ProgressView(source == .freeDOS || preset.dosSetup != nil ? "Installing FreeDOS…" : "Creating…")
            }
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
