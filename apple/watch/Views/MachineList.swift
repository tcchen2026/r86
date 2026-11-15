import SwiftUI

/// The machines on the Watch. Opening one starts it on its screen; swipe a
/// turned-off machine to copy it to the iPhone or delete it.
struct MachineList: View {
    @Environment(Library.self) private var library
    @Environment(\.scenePhase) private var scenePhase
    /// The machine paused when the app left the screen.
    @State private var pausedInBackground: Machine.ID?
    @State private var deleting: Machine?

    private var link: WatchLink { .shared }

    var body: some View {
        NavigationStack {
            List {
                ForEach(library.machines) { m in
                    NavigationLink(value: m.id) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(m.name)
                                .font(.headline)
                                .lineLimit(1)
                            Text(verbatim: "\(m.os) · \(m.memoryText)")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                            if m.isLive {
                                Text(m.statusText)
                                    .font(.caption2)
                                    .foregroundStyle(.green)
                            }
                        }
                    }
                    .swipeActions {
                        if !m.isLive {
                            Button("Delete", systemImage: "trash", role: .destructive) { deleting = m }
                            Button("Copy to iPhone", systemImage: "iphone") { link.send(m) }
                                .tint(.blue)
                                .disabled(!link.available)
                        }
                    }
                }
                ForEach(link.sending) { item in
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Copying \(item.name)")
                            .font(.caption2)
                        ProgressView(item.progress)
                    }
                }
            }
            .overlay {
                if library.machines.isEmpty && link.sending.isEmpty {
                    ContentUnavailableView("No Machines", systemImage: "desktopcomputer",
                                           description: Text("In r86 on your iPhone, turn a machine off and choose Copy to Apple Watch."))
                }
            }
            .navigationTitle("r86")
            .navigationDestination(for: Machine.ID.self) { id in
                WatchScreen(id: id)
            }
        }
        .overlay(alignment: .bottom) {
            WatchToast()
        }
        .confirmationDialog("Delete \(deleting?.name ?? "")?", isPresented: confirmingDelete,
                            titleVisibility: .visible, presenting: deleting) { m in
            Button("Delete", role: .destructive) { library.delete(m.id) }
        } message: { _ in
            Text("Its disk image is deleted from the Watch.")
        }
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background:
                for id in library.sessions.keys where library[id]?.status == .running {
                    library.pause(id)
                    pausedInBackground = id
                }
            case .active:
                if let id = pausedInBackground {
                    pausedInBackground = nil
                    if library[id]?.status == .paused { library.resume(id) }
                }
            default:
                break
            }
        }
    }

    private var confirmingDelete: Binding<Bool> {
        Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })
    }
}

/// The library's latest notice, for three seconds.
struct WatchToast: View {
    @Environment(Library.self) private var library

    var body: some View {
        if let toast = library.toast {
            Label(toast.text, systemImage: toast.systemImage)
                .font(.footnote)
                .padding(8)
                .background(.thinMaterial, in: .rect(cornerRadius: 12))
                .padding(.horizontal, 4)
                .task(id: toast.id) {
                    try? await Task.sleep(for: .seconds(3))
                    if library.toast?.id == toast.id { library.toast = nil }
                }
        }
    }
}
