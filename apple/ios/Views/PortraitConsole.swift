import SwiftUI

/// Tall layouts (iPhone portrait, iPhone Duo closed, iPad portrait): the
/// guest screen on top, then a key row, a trackpad, and the controls.
struct PortraitConsole: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    @Environment(Stage.self) private var stage

    let machine: Machine

    private var running: Bool { machine.status == .running }

    var body: some View {
        VStack(spacing: 10) {
            MachinePill(machine: machine)

            ZStack {
                Color.black
                if let session = library.session(machine.id) {
                    GuestScreen(session: session, keys: stage.keys, scaling: prefs.scaling,
                                smooth: prefs.smoothScaling,
                                touches: running && stage.inputMode == .touch ? .touch : nil,
                                takesKeys: !stage.keyboardUp && stage.sheet == nil,
                                speed: stage.pointerSpeed, haptics: stage.haptics)
                        .opacity(machine.status == .paused ? 0.5 : 1)
                }
                PowerOverlay(machine: machine)
            }
            .aspectRatio(4 / 3, contentMode: .fit)
            .clipShape(.rect(cornerRadius: 16))
            .overlay(alignment: .topTrailing) {
                if prefs.showSpeed, running {
                    SpeedOverlay(machine: machine).padding(8)
                }
            }

            // With the system keyboard up, the key row rides on it instead.
            if running && !stage.keyboardUp {
                KeyRow()
            }

            Group {
                if running, let session = library.session(machine.id) {
                    // Touch needs the guest's r86 tools helper; until it runs,
                    // the trackpad stays.
                    if stage.inputMode == .touch && machine.pointerActive {
                        TouchHint()
                    } else {
                        TrackpadView(session: session, speed: stage.pointerSpeed, haptics: stage.haptics)
                            .overlay {
                                Text(stage.inputMode == .touch
                                     ? "Tapping the screen needs r86 Tools in the guest (More › CD › Install r86 Tools). Until then, use this trackpad."
                                     : "Drag to move · tap to click · two-finger tap to right-click · two fingers to scroll")
                                    .font(.footnote)
                                    .foregroundStyle(.secondary)
                                    .multilineTextAlignment(.center)
                                    .padding()
                                    .allowsHitTesting(false)
                            }
                    }
                } else if machine.status == .stopped {
                    OffDetails(machine: machine)
                } else {
                    Spacer()
                }
            }
            .frame(maxHeight: .infinity)

            SessionTools(machine: machine)
        }
        .padding(.horizontal, 10)
        .padding(.top, 4)
        .padding(.bottom, 6)
    }
}

struct TouchHint: View {
    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "hand.point.up.left").font(.title2)
            Text("Touch").font(.subheadline.weight(.semibold))
            Text("Tap the screen where you want to click; drag to drag; hold to right-click.")
                .font(.footnote)
        }
        .foregroundStyle(.secondary)
        .multilineTextAlignment(.center)
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.white.opacity(0.04), in: .rect(cornerRadius: 26, style: .continuous))
    }
}

/// When the machine is off, the space under the screen shows its settings.
struct OffDetails: View {
    @Environment(Stage.self) private var stage
    let machine: Machine

    var body: some View {
        ScrollView {
            VStack(spacing: 12) {
                SpecGrid(rows: machine.specRows)
                Button("Edit Machine", systemImage: "slider.horizontal.3") {
                    stage.present(.edit(machine.id))
                }
                .buttonStyle(.glass)
            }
            .padding(12)
        }
        .background(Color.white.opacity(0.05), in: .rect(cornerRadius: 26, style: .continuous))
    }
}
