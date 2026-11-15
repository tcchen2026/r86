import SwiftUI

/// A machine's screen; opening it starts the machine. With the guest's r86
/// tools the pointer goes where you touch: tap clicks, a second tap nearby
/// within half a second double-clicks, drag drags, a long press right-clicks.
/// Without them the screen is a trackpad: drag moves the mouse, tap clicks.
/// The Digital Crown scrolls (r86 tools) or presses the up and down arrows.
/// The ⋯ button opens typing and power (WatchControls).
struct WatchScreen: View {
    @Environment(Library.self) private var library
    let id: Machine.ID

    @State private var image: CGImage?
    @State private var size = CGSize.zero
    @State private var touch = Touch()
    @State private var crown = 0.0
    @State private var crownSent = 0.0
    @State private var showControls = false
    /// Another machine is running; the Watch runs one at a time.
    @State private var blockedBy: String?

    var body: some View {
        let machine = library[id]
        ZStack {
            if let blockedBy {
                Text("Turn off \(blockedBy) first.")
                    .font(.footnote)
                    .multilineTextAlignment(.center)
            } else if machine?.status == .stopped {
                VStack(spacing: 8) {
                    Text("\(machine?.name ?? "") is off")
                        .font(.footnote)
                        .multilineTextAlignment(.center)
                    Button("Start", systemImage: "power") { start() }
                }
            } else if let image {
                Image(decorative: image, scale: 1)
                    .resizable()
                    .interpolation(.medium)
                    .aspectRatio(aspect(image), contentMode: .fit)
                    .onGeometryChange(for: CGSize.self) { $0.size } action: { size = $0 }
                    .gesture(drag)
            } else {
                ProgressView()
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .focusable()
        .focusEffectDisabled()
        .digitalCrownRotation($crown, from: -1_000_000, through: 1_000_000, by: 1,
                              sensitivity: .low, isContinuous: false, isHapticFeedbackEnabled: true)
        .onChange(of: crown) { _, value in crowned(value) }
        .navigationTitle(machine?.name ?? "")
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button("Controls", systemImage: "ellipsis") { showControls = true }
                    .disabled(machine?.isLive != true)
            }
        }
        .sheet(isPresented: $showControls) {
            WatchControls(id: id)
        }
        .onAppear(perform: start)
        .task(id: id) {
            // The newest frame, 20 times a second. The library's poll takes
            // frames too, so compare against the session's latest image.
            while !Task.isCancelled {
                if let session = library.session(id) {
                    _ = session.newImage()
                    if let latest = session.image, latest !== image { image = latest }
                }
                try? await Task.sleep(for: .milliseconds(50))
            }
        }
    }

    private func start() {
        guard library[id]?.status == .stopped else { return }
        if let other = library.sessions.keys.first(where: { $0 != id }) {
            blockedBy = library[other]?.name ?? "the running machine"
            return
        }
        blockedBy = nil
        image = nil
        library.start(id)
    }

    /// 320x200, 640x400 and 720x400 at 4:3, as on a CRT.
    private func aspect(_ image: CGImage) -> CGFloat {
        image.height == 200 || image.height == 400 ? 4.0 / 3.0 : CGFloat(image.width) / CGFloat(image.height)
    }

    // MARK: Touch

    /// One finger on the screen.
    private struct Touch {
        var active = false
        var dragging = false
        var began = Date.distantPast
        /// Where the touch went down; a quick second tap snaps to the first.
        var start = CGPoint.zero
        /// Trackpad: the previous location, and motion not yet sent.
        var last = CGPoint.zero
        var remainder = CGSize.zero
        var lastTap: (time: Date, at: CGPoint)?
        /// Where the pointer is, for the crown's wheel.
        var pointer: CGPoint?
    }

    private var drag: some Gesture {
        DragGesture(minimumDistance: 0)
            .onChanged(touchMoved)
            .onEnded(touchEnded)
    }

    private func touchMoved(_ value: DragGesture.Value) {
        guard let session = library.session(id) else { return }
        if !touch.active {
            touch.active = true
            touch.dragging = false
            touch.began = value.time
            touch.start = value.startLocation
            touch.last = value.startLocation
            touch.remainder = .zero
            if let tap = touch.lastTap, value.time.timeIntervalSince(tap.time) < 0.5,
               hypot(value.startLocation.x - tap.at.x, value.startLocation.y - tap.at.y) < 12 {
                touch.start = tap.at
            }
            touch.lastTap = nil
        }
        if !touch.dragging {
            guard hypot(value.translation.width, value.translation.height) > 6 else { return }
            touch.dragging = true
            if session.pointerActive { point(touch.start, buttons: 1) }
        }
        if session.pointerActive {
            point(value.location, buttons: 1)
        } else if let image, size.width > 0, size.height > 0 {
            // Across the screen moves the pointer across the guest screen.
            let dx = (value.location.x - touch.last.x) * CGFloat(image.width) / size.width + touch.remainder.width
            let dy = (value.location.y - touch.last.y) * CGFloat(image.height) / size.height + touch.remainder.height
            let (sx, sy) = (dx.rounded(.towardZero), dy.rounded(.towardZero))
            touch.remainder = CGSize(width: dx - sx, height: dy - sy)
            touch.last = value.location
            if sx != 0 || sy != 0 { session.mouse(dx: Int32(sx), dy: Int32(sy), buttons: 0) }
        }
    }

    private func touchEnded(_ value: DragGesture.Value) {
        defer { touch.active = false }
        guard let session = library.session(id) else { return }
        if touch.dragging {
            if session.pointerActive { point(value.location, buttons: 0) }
            return
        }
        let long = value.time.timeIntervalSince(touch.began) >= 0.5
        let button: UInt32 = long ? 2 : 1
        if session.pointerActive {
            point(touch.start, buttons: button)
            point(touch.start, buttons: 0)
        } else {
            session.mouse(dx: 0, dy: 0, buttons: button)
            session.mouse(dx: 0, dy: 0, buttons: 0)
        }
        if !long { touch.lastTap = (value.time, touch.start) }
    }

    /// Absolute position for the r86 tools helper.
    private func point(_ p: CGPoint, buttons: UInt32, wheel: Int16 = 0) {
        guard let session = library.session(id), size.width > 0, size.height > 0 else { return }
        touch.pointer = p
        let x = UInt16((min(max(p.x / size.width, 0), 1) * 65535).rounded())
        let y = UInt16((min(max(p.y / size.height, 0), 1) * 65535).rounded())
        session.pointer(x: x, y: y, buttons: buttons, wheel: wheel)
    }

    // MARK: Digital Crown

    /// Whole detents since the last call: wheel steps (positive scrolls
    /// down) or arrow keys.
    private func crowned(_ value: Double) {
        let steps = Int((value - crownSent).rounded(.towardZero))
        guard steps != 0 else { return }
        crownSent += Double(steps)
        guard let session = library.session(id) else { return }
        if session.pointerActive {
            let at = touch.pointer ?? CGPoint(x: size.width / 2, y: size.height / 2)
            point(at, buttons: 0, wheel: Int16(max(-127, min(127, steps))))
        } else {
            let key = steps > 0 ? HID.down : HID.up
            for _ in 0..<min(abs(steps), 8) {
                session.hidKey(key, down: true)
                session.hidKey(key, down: false)
            }
        }
    }
}
