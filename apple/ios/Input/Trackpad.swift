import SwiftUI
import UIKit

/// Turns touches into pointer input like a laptop trackpad: one finger moves
/// the pointer, a tap clicks, a two-finger tap right-clicks, a double tap
/// held down drags, and two fingers scroll.
///
/// While the guest's r86 tools helper runs, the pointer is absolute: this
/// keeps the position and sends it, so scrolling works and Windows' mouse
/// acceleration stays out of the way. Without the helper it sends PS/2
/// motion, with no wheel.
@MainActor
final class TrackpadInterpreter {
    weak var session: Session?
    var speed: CGFloat = 1
    /// A click was sent: a light haptic tap.
    var onClick: () -> Void = {}

    /// Finger travel, in points, across the whole guest screen at speed 1.
    private let span: CGFloat = 420
    private var last: [ObjectIdentifier: CGPoint] = [:]
    private var downTime: TimeInterval = 0
    private var travel: CGFloat = 0
    private var fingers = 0
    private var dragging = false
    private var lastTap: (time: TimeInterval, at: CGPoint)?
    private var buttons: UInt32 = 0
    private var remainder = CGPoint.zero
    private var wheel: CGFloat = 0
    private var position = CGPoint(x: 0.5, y: 0.5)

    func began(_ touches: Set<UITouch>, in view: UIView) {
        let first = last.isEmpty
        for touch in touches { last[ObjectIdentifier(touch)] = touch.location(in: view) }
        guard first, let touch = touches.first else {
            fingers = max(fingers, last.count)
            return
        }
        downTime = touch.timestamp
        travel = 0
        fingers = last.count
        let p = touch.location(in: view)
        if let tap = lastTap, downTime - tap.time < 0.3, hypot(p.x - tap.at.x, p.y - tap.at.y) < 40 {
            dragging = true
            buttons = 1
            send()
        }
    }

    func moved(_ touches: Set<UITouch>, in view: UIView) {
        var delta = CGPoint.zero
        var count: CGFloat = 0
        for touch in touches {
            let id = ObjectIdentifier(touch)
            guard let old = last[id] else { continue }
            let p = touch.location(in: view)
            delta.x += p.x - old.x
            delta.y += p.y - old.y
            count += 1
            last[id] = p
        }
        guard count > 0 else { return }
        delta.x /= count
        delta.y /= count
        travel += abs(delta.x) + abs(delta.y)
        if last.count >= 2 && !dragging {
            scroll(delta.y)
        } else {
            send(dx: delta.x, dy: delta.y)
        }
    }

    func ended(_ touches: Set<UITouch>, cancelled: Bool = false) {
        let time = touches.first?.timestamp ?? downTime
        let at = touches.first.map { last[ObjectIdentifier($0)] ?? .zero } ?? .zero
        for touch in touches { last[ObjectIdentifier(touch)] = nil }
        guard last.isEmpty else { return }
        if dragging {
            dragging = false
            buttons = 0
            send()
        } else if !cancelled && travel < 10 && time - downTime < 0.3 {
            click(fingers >= 2 ? 2 : 1)
            lastTap = fingers >= 2 ? nil : (time: time, at: at)
            return
        }
        lastTap = nil
    }

    /// Everything up: the view went away or focus left.
    func reset() {
        last.removeAll()
        if buttons != 0 {
            buttons = 0
            send()
        }
        dragging = false
        lastTap = nil
    }

    private func click(_ bit: UInt32) {
        buttons = bit
        send()
        buttons = 0
        send()
        onClick()
    }

    private func scroll(_ dy: CGFloat) {
        // Natural scrolling: fingers moving up scroll the guest down.
        wheel -= dy / 12
        let steps = wheel.rounded(.towardZero)
        wheel -= steps
        if steps != 0 { send(wheel: Int16(max(-127, min(127, steps)))) }
    }

    private func send(dx: CGFloat = 0, dy: CGFloat = 0, wheel steps: Int16 = 0) {
        guard let session else { return }
        if session.pointerActive {
            position.x = min(1, max(0, position.x + dx * speed / span))
            position.y = min(1, max(0, position.y + dy * speed / (span * 0.7)))
            session.pointer(x: UInt16(position.x * 65535), y: UInt16(position.y * 65535),
                            buttons: buttons, wheel: steps)
        } else {
            remainder.x += dx * speed * 1.5
            remainder.y += dy * speed * 1.5
            let x = remainder.x.rounded(.towardZero)
            let y = remainder.y.rounded(.towardZero)
            remainder.x -= x
            remainder.y -= y
            session.mouse(dx: Int32(x), dy: Int32(y), buttons: buttons)
        }
    }
}

/// A trackpad surface: the portrait layout's pad under the screen.
struct TrackpadView: UIViewRepresentable {
    let session: Session
    var speed: CGFloat = 1
    var haptics = true

    func makeUIView(context: Context) -> TrackpadUIView {
        let view = TrackpadUIView()
        update(view)
        return view
    }

    func updateUIView(_ view: TrackpadUIView, context: Context) {
        update(view)
    }

    static func dismantleUIView(_ view: TrackpadUIView, coordinator: ()) {
        view.pad.reset()
    }

    private func update(_ view: TrackpadUIView) {
        view.pad.session = session
        view.pad.speed = speed
        view.haptics = haptics
    }
}

final class TrackpadUIView: UIView {
    let pad = TrackpadInterpreter()
    var haptics = true
    private let feedback = UIImpactFeedbackGenerator(style: .light)

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = UIColor(white: 1, alpha: 0.06)
        layer.cornerRadius = 26
        layer.cornerCurve = .continuous
        pad.onClick = { [weak self] in
            guard let self, self.haptics else { return }
            self.feedback.impactOccurred()
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        backgroundColor = UIColor(white: 1, alpha: 0.10)
        pad.began(touches, in: self)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        pad.moved(touches, in: self)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        pad.ended(touches)
        if event?.allTouches?.allSatisfy({ $0.phase == .ended || $0.phase == .cancelled }) ?? true {
            backgroundColor = UIColor(white: 1, alpha: 0.06)
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        pad.ended(touches, cancelled: true)
        backgroundColor = UIColor(white: 1, alpha: 0.06)
    }
}
