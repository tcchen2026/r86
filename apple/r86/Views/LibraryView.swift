import AppKit
import SwiftUI

/// The Library window: machines on the left, the selected one on the right.
/// Starting a machine opens it in its own window.
struct LibraryView: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    @Environment(\.openWindow) private var openWindow

    @State private var selection: Machine.ID?
    @State private var editing: Machine?
    @State private var pendingDelete: Machine?
    @State private var showDelete = false

    var body: some View {
        @Bindable var bindable = library
        NavigationSplitView {
            List(selection: $selection) {
                ForEach(library.machines) { machine in
                    SidebarRow(machine: machine).tag(machine.id)
                }
                if !library.problems.isEmpty {
                    Section("Unreadable") {
                        ForEach(library.problems, id: \.self) { problem in
                            Label(problem, systemImage: "exclamationmark.triangle")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            }
            .contextMenu(forSelectionType: Machine.ID.self) { ids in
                if let id = ids.first, let m = library[id] {
                    rowMenu(m)
                }
            } primaryAction: { ids in
                if let id = ids.first { runOrShow(id) }
            }
            .navigationSplitViewColumnWidth(min: 220, ideal: 250, max: 340)
            .toolbar {
                ToolbarItem {
                    Button("New Machine", systemImage: "plus") { library.requestNewMachine = true }
                        .help("New Machine (⌘N)")
                }
            }
        } detail: {
            if let m = library[selection] {
                MachineDetailView(machine: m) { runOrShow(m.id) }
                    .navigationTitle(m.name)
                    .toolbar { detailToolbar(m) }
            } else {
                ContentUnavailableView("No Machine Selected", systemImage: "desktopcomputer",
                                       description: Text("Select a machine, or create one with ⌘N."))
            }
        }
        .overlay(alignment: .top) { ToastOverlay() }
        .modifier(FocusedMachineValue(id: selection))
        .sheet(isPresented: $bindable.requestNewMachine) {
            NewMachineSheet { id, start in
                selection = id
                if start { runOrShow(id) }
            }
            .environment(library)
        }
        .sheet(item: $editing) { m in
            EditMachineSheet(machine: m)
                .environment(library)
        }
        .confirmationDialog("Delete this machine?", isPresented: $showDelete, presenting: pendingDelete) { m in
            Button("Delete \(m.name)", role: .destructive) { library.delete(m.id) }
        } message: { m in
            Text("Its folder, including a disk stored there, is deleted. CD images stay where they are.")
        }
        .onChange(of: library.requestEditID) { _, id in
            guard let id, let m = library[id] else { return }
            selection = id
            editing = m
            library.requestEditID = nil
        }
        .onAppear {
            if selection == nil { selection = library.machines.first?.id }
        }
    }

    @ViewBuilder
    private func rowMenu(_ m: Machine) -> some View {
        Button(m.isLive ? "Show Window" : "Start") { runOrShow(m.id) }
        Button("Edit…") { editing = m }
            .disabled(m.isLive)
        Button("Show in Finder") {
            NSWorkspace.shared.activateFileViewerSelecting([URL(filePath: m.directory)])
        }
        Divider()
        Button("Delete…", role: .destructive) {
            pendingDelete = m
            showDelete = true
        }
        .disabled(m.isLive)
    }

    @ToolbarContentBuilder
    private func detailToolbar(_ m: Machine) -> some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            if m.status == .running {
                Button("Pause", systemImage: "pause.fill") { library.pause(m.id) }
            } else {
                Button(m.status == .paused ? "Continue" : "Start", systemImage: "play.fill") {
                    runOrShow(m.id)
                }
            }
        }
        ToolbarItemGroup(placement: .primaryAction) {
            Button("Edit", systemImage: "slider.horizontal.3") { editing = m }
                .disabled(m.isLive)
            Button("Delete", systemImage: "trash") {
                pendingDelete = m
                showDelete = true
            }
            .disabled(m.isLive)
        }
    }

    private func runOrShow(_ id: Machine.ID) {
        guard let m = library[id] else { return }
        if m.status == .paused {
            library.resume(id)
        } else if !m.isLive {
            library.start(id)
        }
        if library.session(id) != nil {
            openWindow(id: "machine", value: id)
        }
    }
}

// MARK: - Sidebar row

struct SidebarRow: View {
    let machine: Machine

    var body: some View {
        HStack(spacing: 10) {
            MachineThumbnail(machine: machine, cornerRadius: 5)
                .frame(width: 44)
            VStack(alignment: .leading, spacing: 2) {
                Text(machine.name).lineLimit(1)
                Text(verbatim: "\(machine.os) · \(machine.memoryText)")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            StatusDot(status: machine.status)
        }
        .padding(.vertical, 3)
    }
}

// MARK: - Detail

struct MachineDetailView: View {
    let machine: Machine
    var run: () -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                ZStack {
                    MachineThumbnail(machine: machine, cornerRadius: 14)
                        .brightness(machine.isLive ? 0 : -0.2)
                    if machine.isLive {
                        Button("Show Window", systemImage: "macwindow", action: run)
                            .buttonStyle(.glass)
                            .controlSize(.large)
                    } else {
                        Button(action: run) {
                            Image(systemName: "play.fill")
                                .font(.system(size: 30, weight: .semibold))
                                .frame(width: 76, height: 76)
                                .contentShape(.circle)
                        }
                        .buttonStyle(.plain)
                        .glassEffect(.regular.interactive(), in: .circle)
                        .accessibilityLabel("Start \(machine.name)")
                    }
                }
                .environment(\.colorScheme, .dark)

                HStack(alignment: .firstTextBaseline, spacing: 12) {
                    Text(machine.name).font(.largeTitle.bold())
                    StatusLabel(machine: machine)
                }

                SpecGrid(rows: machine.specRows)

                if !machine.notes.isEmpty {
                    Text(machine.notes)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
            .padding(24)
            .frame(maxWidth: 640)
            .frame(maxWidth: .infinity)
        }
    }
}

// MARK: - Focus

/// Lets the Machine menu act on the selected machine while the Library is in front.
struct FocusedMachineValue: ViewModifier {
    let id: Machine.ID?

    func body(content: Content) -> some View {
        if let id {
            content.focusedSceneValue(\.machineID, id)
        } else {
            content
        }
    }
}
