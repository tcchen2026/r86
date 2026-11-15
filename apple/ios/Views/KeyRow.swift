import SwiftUI

/// Keys the system keyboard lacks, above it or under the guest screen.
/// Ctrl, Alt, Win and Shift latch for the next key (double tap locks); the
/// others go down while touched, so arrows repeat when held.
struct KeyRow: View {
    @Environment(Stage.self) private var stage

    private struct Key: Identifiable {
        let label: String
        let usage: UInt16
        var modifier = false
        var id: UInt16 { usage }
    }

    private let keys: [Key] = [
        Key(label: "esc", usage: HID.escape),
        Key(label: "tab", usage: HID.tab),
        Key(label: "ctrl", usage: HID.leftControl, modifier: true),
        Key(label: "alt", usage: HID.leftAlt, modifier: true),
        Key(label: "win", usage: HID.leftGUI, modifier: true),
        Key(label: "shift", usage: HID.leftShift, modifier: true),
        Key(label: "←", usage: HID.left),
        Key(label: "↑", usage: HID.up),
        Key(label: "↓", usage: HID.down),
        Key(label: "→", usage: HID.right),
        Key(label: "del", usage: HID.delete),
    ]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            GlassEffectContainer(spacing: 6) {
                HStack(spacing: 6) {
                    ForEach(keys) { key in
                        if key.modifier {
                            ModifierKey(label: key.label, usage: key.usage, keys: stage.keys)
                        } else {
                            HoldKey(label: key.label, usage: key.usage, keys: stage.keys)
                        }
                    }
                    FunctionKeys(keys: stage.keys)
                }
                .padding(.horizontal, 2)
            }
        }
        .scrollClipDisabled()
        .fixedSize(horizontal: false, vertical: true)
    }
}

private struct KeyCap: View {
    let label: String
    var lit = false

    var body: some View {
        Text(label)
            .font(.system(size: 15, weight: .medium))
            .frame(minWidth: 40, minHeight: 38)
            .padding(.horizontal, 4)
            .foregroundStyle(lit ? AnyShapeStyle(.tint) : AnyShapeStyle(.primary))
            .contentShape(.capsule)
            .glassEffect(.regular.interactive(), in: .capsule)
    }
}

/// Down while touched, up when lifted. A button, so a swipe that starts on
/// a key still scrolls the row instead of pressing the key.
private struct HoldKey: View {
    let label: String
    let usage: UInt16
    let keys: KeyInput

    var body: some View {
        Button(label) {}
            .buttonStyle(Held(label: label) { pressed in pressed ? keys.down(usage) : keys.up(usage) })
    }

    private struct Held: ButtonStyle {
        let label: String
        let changed: (Bool) -> Void

        func makeBody(configuration: Configuration) -> some View {
            KeyCap(label: label, lit: configuration.isPressed)
                .onChange(of: configuration.isPressed) { _, pressed in changed(pressed) }
        }
    }
}

/// Tap to latch for the next key; tap again quickly to lock.
private struct ModifierKey: View {
    let label: String
    let usage: UInt16
    let keys: KeyInput
    @State private var lastTap = Date.distantPast

    var body: some View {
        KeyCap(label: label, lit: keys.isLatched(usage))
            .onTapGesture {
                let now = Date()
                keys.cycle(usage, double: now.timeIntervalSince(lastTap) < 0.35)
                lastTap = now
            }
            .accessibilityLabel(label)
            .accessibilityAddTraits(.isButton)
    }
}

/// F1 to F12 and Ctrl+Alt+Del in a menu.
private struct FunctionKeys: View {
    @Environment(Library.self) private var library
    @Environment(Stage.self) private var stage
    let keys: KeyInput

    var body: some View {
        Menu {
            ForEach(1...12, id: \.self) { n in
                Button("F\(n)") { keys.tap(HID.f(n)) }
            }
            Divider()
            Button("Ctrl+Alt+Del") {
                if let id = stage.current(library)?.id { library.ctrlAltDel(id) }
            }
        } label: {
            KeyCap(label: "fn")
        }
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .accessibilityLabel("Function keys")
    }
}
