import QuickLook
import SwiftUI
import UIKit

/// The app is the machine on screen; the name at the top switches machines.
///
/// The layout follows the window, not the device:
/// - an iPhone Duo folded like a laptop: the guest above the fold, the deck
///   (keyboard or trackpad) below it (LaptopConsole);
/// - wide (iPhone landscape, iPad, the Duo open flat): the guest fills the
///   screen under a floating command bar (FullScreenSession);
/// - tall (iPhone portrait, the Duo closed, iPad portrait): the guest on top,
///   key row and trackpad below (PortraitConsole).
struct RootView: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    @Environment(Stage.self) private var stage
    @Environment(\.scenePhase) private var scenePhase
    /// The machine paused when the app went to the background.
    @State private var pausedInBackground: Machine.ID?

    var body: some View {
        @Bindable var stage = stage
        @Bindable var library = library
        GeometryReader { proxy in
            let fold = Fold.laptop(in: proxy)
            let wide = proxy.size.width > proxy.size.height * 0.87
            ZStack {
                Color.black
                if let machine = stage.current(library) {
                    if let fold {
                        LaptopConsole(machine: machine, fold: fold, size: proxy.size)
                    } else if wide {
                        FullScreenSession(machine: machine)
                    } else {
                        PortraitConsole(machine: machine)
                            .padding(.top, proxy.safeAreaInsets.top)
                            .padding(.bottom, proxy.safeAreaInsets.bottom)
                    }
                } else {
                    EmptyMachines()
                }
            }
            .animation(.smooth(duration: 0.3), value: fold != nil)
        }
        .ignoresSafeArea()
        .overlay(alignment: .top) {
            ToastOverlay(topPadding: 56)
        }
        .background {
            TypingField(isActive: $stage.keyboardUp, keys: stage.keys) {
                KeyRow()
                    .padding(.horizontal, 8)
                    .environment(library)
                    .environment(stage)
                    .environment(\.colorScheme, .dark)
            }
            .frame(width: 1, height: 1)
            .opacity(0.01)
            .accessibilityHidden(true)
        }
        .environment(\.colorScheme, .dark)
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        // Typing on the deck's bottom row must not leave the app.
        .defersSystemGestures(on: .all)
        .sheet(item: $stage.sheet) { sheet in
            sheetContent(sheet)
                .environment(library)
                .environment(prefs)
                .environment(stage)
        }
        .confirmationDialog(switchTitle, isPresented: switching, titleVisibility: .visible) {
            Button("Turn Off and Switch", role: .destructive) { stage.confirmSwitch(library) }
            Button("Cancel", role: .cancel) { stage.pendingSwitch = nil }
        } message: {
            Text("r86 cannot save a running machine. The guest does not shut down first; anything it has not saved is lost.")
        }
        .fileImporter(isPresented: $stage.importingCD, allowedContentTypes: CDImage.types) { result in
            if case .success(let url) = result, let id = stage.current(library)?.id {
                library.changeCD(id, url)
            }
        }
        .quickLookPreview($library.logToShow)
        .onChange(of: scenePhase) { _, phase in
            sceneChanged(phase)
        }
        .onChange(of: stage.current(library)?.status) { _, status in
            // Keep the screen on while a machine runs.
            UIApplication.shared.isIdleTimerDisabled = status == .running
            if status != .running { stage.keyboardUp = false }
        }
    }

    private var running: Machine.ID? {
        library.sessions.keys.first
    }

    private var switching: Binding<Bool> {
        Binding(get: { stage.pendingSwitch != nil },
                set: { if !$0 { stage.pendingSwitch = nil } })
    }

    private var switchTitle: String {
        let name = running.flatMap { library[$0]?.name } ?? "the running machine"
        return "Turn off \(name)?"
    }

    @ViewBuilder
    private func sheetContent(_ sheet: RootSheet) -> some View {
        switch sheet {
        case .newMachine:
            NewMachineSheet { id, start in
                stage.select(id, library: library, start: start)
            }
        case .edit(let id):
            if let m = library[id] {
                EditMachineSheet(machine: m)
            }
        case .settings:
            SettingsSheet()
        }
    }

    /// iOS suspends apps in the background: pause the machine, and continue
    /// it on return. If iOS ends the app meanwhile, the machine is off; what
    /// the guest wrote to disk stays.
    private func sceneChanged(_ phase: ScenePhase) {
        switch phase {
        case .background:
            stage.keys.releaseAll()
            if let m = stage.current(library), m.status == .running {
                library.pause(m.id)
                pausedInBackground = m.id
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

// MARK: - Empty library

struct EmptyMachines: View {
    @Environment(Stage.self) private var stage

    var body: some View {
        ContentUnavailableView {
            Label("No Machines", systemImage: "shippingbox")
        } description: {
            Text("Create one from an installation CD image, or start with FreeDOS, which comes with r86. Machines are kept in Files › r86 › VMs.")
        } actions: {
            Button("New Machine") { stage.present(.newMachine) }
                .buttonStyle(.glassProminent)
            Button("Settings") { stage.present(.settings) }
                .buttonStyle(.glass)
        }
    }
}
