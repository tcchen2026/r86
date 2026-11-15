import SwiftUI

/// Wide layouts (iPhone landscape, iPad, iPhone Duo open flat): the guest
/// takes the whole screen. Controls live in a small floating bar that
/// collapses to a name tag, hides itself and can be dragged along the top.
struct FullScreenSession: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    @Environment(Stage.self) private var stage

    let machine: Machine

    var body: some View {
        ZStack(alignment: .top) {
            Color.black

            if let session = library.session(machine.id) {
                GuestScreen(session: session, keys: stage.keys, scaling: prefs.scaling,
                            smooth: prefs.smoothScaling,
                            touches: machine.status == .running ? stage.inputMode : nil,
                            fallback: .trackpad,
                            takesKeys: !stage.keyboardUp && stage.sheet == nil,
                            speed: stage.pointerSpeed, haptics: stage.haptics)
                    .opacity(machine.status == .paused ? 0.5 : 1)
            }
            PowerOverlay(machine: machine)
                .frame(maxWidth: .infinity, maxHeight: .infinity)

            FloatingCommandBar(machine: machine)
                .padding(.top, 10)
        }
        .overlay(alignment: .topTrailing) {
            if prefs.showSpeed, machine.status == .running {
                SpeedOverlay(machine: machine).padding(.top, 64).padding(.trailing, 12)
            }
        }
    }
}

// MARK: - Floating command bar

struct FloatingCommandBar: View {
    @Environment(Stage.self) private var stage

    let machine: Machine
    /// The laptop deck has its own keys.
    var compact = false

    @Namespace private var glass
    @State private var expanded = true
    @State private var anchor: BarAnchor = .center
    @State private var dragX: CGFloat = 0
    @State private var poke = 0

    enum BarAnchor {
        case leading, center, trailing

        var alignment: Alignment {
            switch self {
            case .leading: return .leading
            case .center: return .center
            case .trailing: return .trailing
            }
        }

        var fraction: CGFloat {
            switch self {
            case .leading: return 0.18
            case .center: return 0.5
            case .trailing: return 0.82
            }
        }
    }

    var body: some View {
        GeometryReader { geo in
            GlassEffectContainer(spacing: 8) {
                if expanded {
                    expandedBar
                } else {
                    handle
                }
            }
            .offset(x: dragX)
            .frame(width: geo.size.width, height: geo.size.height, alignment: anchor.alignment)
            .simultaneousGesture(drag(width: geo.size.width))
        }
        .frame(height: 48)
        // Clear of the rounded corners when the bar sits at either end.
        .padding(.horizontal, 28)
        .animation(.spring(duration: 0.35, bounce: 0.2), value: expanded)
        .task(id: autoHideKey) {
            guard expanded, !stage.showPicker, !stage.keyboardUp, machine.status == .running else { return }
            try? await Task.sleep(for: .seconds(4))
            guard !Task.isCancelled else { return }
            expanded = false
        }
    }

    /// Any change here restarts the four-second auto-hide timer.
    private var autoHideKey: String {
        "\(expanded)-\(poke)-\(stage.showPicker)-\(stage.keyboardUp)-\(machine.id)-\(machine.status)"
    }

    private var expandedBar: some View {
        HStack(spacing: 8) {
            MachinePill(machine: machine)
                .glassEffectID("name", in: glass)
                .simultaneousGesture(TapGesture().onEnded { poke += 1 })

            SessionTools(machine: machine,
                         onCollapse: { expanded = false },
                         onInteract: { poke += 1 },
                         compact: compact)
                .glassEffectID("tools", in: glass)
        }
    }

    private var handle: some View {
        Button {
            expanded = true
        } label: {
            HStack(spacing: 6) {
                StatusDot(status: machine.status)
                Text(machine.name)
                    .font(.footnote.weight(.semibold))
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .frame(height: 30)
            .contentShape(.capsule)
        }
        .buttonStyle(.plain)
        .glassEffect(.regular.interactive(), in: .capsule)
        .glassEffectID("name", in: glass)
        .opacity(0.85)
        .accessibilityLabel("Show controls for \(machine.name)")
    }

    private func drag(width: CGFloat) -> some Gesture {
        DragGesture(minimumDistance: 12)
            .onChanged { value in
                dragX = value.translation.width
                poke += 1
            }
            .onEnded { value in
                let x = anchor.fraction * width + value.translation.width
                withAnimation(.spring(duration: 0.35, bounce: 0.2)) {
                    if x < width / 3 {
                        anchor = .leading
                    } else if x > width * 2 / 3 {
                        anchor = .trailing
                    } else {
                        anchor = .center
                    }
                    dragX = 0
                }
            }
    }
}
