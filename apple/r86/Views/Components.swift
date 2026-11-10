import SwiftUI

// MARK: - Status

struct StatusDot: View {
    let status: MachineStatus

    var body: some View {
        Circle()
            .fill(fill)
            .frame(width: 8, height: 8)
            .accessibilityHidden(true)
    }

    private var fill: Color {
        switch status {
        case .running: return .green
        case .paused: return .orange
        case .stopped: return Color.secondary.opacity(0.6)
        }
    }
}

struct StatusLabel: View {
    let machine: Machine

    var body: some View {
        HStack(spacing: 6) {
            StatusDot(status: machine.status)
            Text(machine.statusText).monospacedDigit()
        }
        .font(.subheadline)
        .foregroundStyle(.secondary)
    }
}

// MARK: - Spec table

struct SpecGrid: View {
    let rows: [(String, String)]

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 0) {
            ForEach(Array(rows.enumerated()), id: \.offset) { index, row in
                if index > 0 { Divider() }
                GridRow {
                    Text(row.0).foregroundStyle(.secondary)
                    Text(row.1)
                        .font(.callout.monospaced())
                        .multilineTextAlignment(.trailing)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .padding(.vertical, 8)
            }
        }
        .padding(.horizontal, 14)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 12))
    }
}

// MARK: - Screens

/// The running machine's screen, or black, at 16:10.
struct MachineThumbnail: View {
    @Environment(Library.self) private var library
    let machine: Machine
    var cornerRadius: CGFloat = 6

    var body: some View {
        ZStack {
            Color.black
            if let image = library.session(machine.id)?.thumbnail {
                Image(decorative: image, scale: 1)
                    .resizable()
                    .interpolation(.medium)
                    .aspectRatio(contentMode: .fit)
            }
        }
        .aspectRatio(16 / 10, contentMode: .fit)
        .clipShape(.rect(cornerRadius: cornerRadius))
        .overlay {
            RoundedRectangle(cornerRadius: cornerRadius).strokeBorder(.separator, lineWidth: 0.5)
        }
    }
}

struct SpeedOverlay: View {
    let machine: Machine

    var body: some View {
        Text(verbatim: "\(Format.speed(machine.instructionsPerSecond)) · \(machine.displayWidth)×\(machine.displayHeight)")
            .font(.system(size: 11, weight: .medium, design: .monospaced))
            .foregroundStyle(Color(red: 0.72, green: 0.96, blue: 0.81))
            .padding(.horizontal, 7)
            .padding(.vertical, 4)
            .background(Color.black.opacity(0.55), in: .rect(cornerRadius: 6))
            .allowsHitTesting(false)
    }
}

// MARK: - Settings rows

/// Memory doubling from 16 MiB to 2 GiB, the range r86 accepts.
struct MemoryRow: View {
    @Binding var memoryMiB: Int

    var body: some View {
        Stepper(onIncrement: { memoryMiB = min(memoryMiB * 2, 2048) },
                onDecrement: { memoryMiB = max(memoryMiB / 2, 16) }) {
            LabeledContent("Memory") {
                Text(Format.size(mib: memoryMiB)).monospacedDigit()
            }
        }
    }
}

struct DiskRow: View {
    @Binding var diskMiB: Int

    var body: some View {
        Stepper(onIncrement: { diskMiB = min(diskMiB * 2, 131_072) },
                onDecrement: { diskMiB = max(diskMiB / 2, 64) }) {
            LabeledContent("Disk") {
                Text(Format.size(mib: diskMiB)).monospacedDigit()
            }
        }
    }
}

struct VolumePicker: View {
    @Binding var volume: Int

    var body: some View {
        Picker("Sound", selection: $volume) {
            ForEach([100, 75, 50, 25, 0], id: \.self) { level in
                Text(Format.volume(level)).tag(level)
            }
        }
    }
}

// MARK: - Power overlay (on top of the screen when the machine isn't running)

struct PowerOverlay: View {
    @Environment(Library.self) private var library
    let machine: Machine

    var body: some View {
        switch machine.status {
        case .stopped:
            VStack(spacing: 10) {
                Button {
                    library.start(machine.id)
                } label: {
                    Image(systemName: "power")
                        .font(.system(size: 30, weight: .semibold))
                        .frame(width: 76, height: 76)
                        .contentShape(.circle)
                }
                .buttonStyle(.plain)
                .glassEffect(.regular.interactive(), in: .circle)
                .accessibilityLabel("Start \(machine.name)")

                Text("\(machine.name) is off")
                    .font(.title3.weight(.semibold))
                Text("\(machine.os) · \(machine.memoryText) · \(machine.diskText)")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
            }
            .foregroundStyle(.white)
            .environment(\.colorScheme, .dark)
        case .paused:
            Button("Continue", systemImage: "play.fill") { library.resume(machine.id) }
                .buttonStyle(.glass)
                .environment(\.colorScheme, .dark)
        case .running:
            EmptyView()
        }
    }
}

// MARK: - Toast

struct ToastOverlay: View {
    @Environment(Library.self) private var library
    var topPadding: CGFloat = 12

    var body: some View {
        ZStack {
            if let toast = library.toast {
                Label(toast.text, systemImage: toast.systemImage)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(2)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .glassEffect(.regular, in: .capsule)
                    .id(toast.id)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        .padding(.top, topPadding)
        .padding(.horizontal, 16)
        .animation(.spring(duration: 0.35), value: library.toast)
        .task(id: library.toast?.id) {
            guard library.toast != nil else { return }
            try? await Task.sleep(for: .seconds(2.4))
            if !Task.isCancelled { library.toast = nil }
        }
        .allowsHitTesting(false)
    }
}
