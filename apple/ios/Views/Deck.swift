import SwiftUI
import UIKit

/// The laptop deck: the lower half of an iPhone Duo folded like a laptop.
/// A full keyboard, an F-row over a 60% layout, 15 units across; or, after
/// a tap on ▦, a trackpad over the whole half. Holding ▦ makes the rest of
/// the deck a trackpad until it is released.
struct DeckView: UIViewRepresentable {
    let session: Session
    let keys: KeyInput
    @Binding var trackpadMode: Bool
    var speed: CGFloat = 1
    var haptics = true
    var onCtrlAltDel: () -> Void = {}

    func makeUIView(context: Context) -> DeckUIView {
        DeckUIView()
    }

    func updateUIView(_ view: DeckUIView, context: Context) {
        view.keys = keys
        view.pad.session = session
        view.pad.speed = speed
        view.haptics = haptics
        view.onCtrlAltDel = onCtrlAltDel
        let binding = $trackpadMode
        view.onTrackpadMode = { on in
            DispatchQueue.main.async { binding.wrappedValue = on }
        }
        if view.trackpadMode != trackpadMode { view.trackpadMode = trackpadMode }
    }

    static func dismantleUIView(_ view: DeckUIView, coordinator: ()) {
        view.releaseAll()
    }
}

final class DeckUIView: UIView {
    struct Key {
        enum Kind { case key, modifier, fn, trackpad }

        let label: String
        var shifted: String? = nil
        var fnLabel: String? = nil
        var usage: UInt16 = 0
        var fnUsage: UInt16? = nil
        var width: CGFloat = 1
        var kind = Kind.key
    }

    var keys: KeyInput?
    let pad = TrackpadInterpreter()
    var haptics = true
    var onCtrlAltDel: () -> Void = {}
    var onTrackpadMode: (Bool) -> Void = { _ in }
    var trackpadMode = false {
        didSet {
            if trackpadMode { releaseKeys() }
            setNeedsDisplay()
        }
    }

    private let rows: [[Key]] = DeckUIView.layout()
    /// Row heights in key units: the F-row is shorter.
    private let rowHeights: [CGFloat] = [0.8, 1, 1, 1, 1, 1]
    private var frames: [[CGRect]] = []
    private let feedback = UIImpactFeedbackGenerator(style: .light)

    /// Key touches: touch → (row, column) and the usage it sent down, if any.
    private var touched: [ObjectIdentifier: (row: Int, column: Int, usage: UInt16?)] = [:]
    /// Modifiers held by a finger: usage → sent to the guest yet. A modifier
    /// goes down only when another key needs it; a tap alone latches it.
    private var heldModifiers: [UInt16: Bool] = [:]
    private var lastModifierTap: [UInt16: TimeInterval] = [:]
    private var fnHeld = false
    private var fnUsed = false
    private var fnLatched = false
    /// Trackpad touches: in trackpad mode, or while ▦ is held.
    private var padTouches: Set<ObjectIdentifier> = []
    private var trackpadKey: ObjectIdentifier?
    private var momentary = false
    private var momentaryTimer: Timer?
    private var keyboardButtonTouch: ObjectIdentifier?

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        isOpaque = false
        backgroundColor = .clear
        contentMode = .redraw
        pad.onClick = { [weak self] in self?.tick() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    // MARK: Layout

    private static func layout() -> [[Key]] {
        func letters(_ s: String) -> [Key] {
            s.map { c in Key(label: String(c).uppercased(), usage: HID.letter(c)) }
        }
        func symbol(_ label: String, _ shifted: String, _ usage: UInt16) -> Key {
            Key(label: label, shifted: shifted, usage: usage)
        }
        let digits = Array("1234567890")
        let shiftedDigits = Array("!@#$%^&*()")
        var row1 = [Key(label: "`", shifted: "~", fnLabel: "prt", usage: HID.grave, fnUsage: HID.printScreen)]
        for (i, d) in digits.enumerated() {
            row1.append(Key(label: String(d), shifted: String(shiftedDigits[i]),
                            usage: d == "0" ? HID.zero : HID.one + UInt16(i)))
        }
        row1 += [symbol("-", "_", HID.minus), symbol("=", "+", HID.equal),
                 Key(label: "⌫", usage: HID.backspace, width: 2)]

        var row0 = [Key(label: "esc", fnLabel: "C·A·D", usage: HID.escape)]
        for n in 1...12 { row0.append(Key(label: "F\(n)", usage: HID.f(n))) }
        row0 += [Key(label: "del", fnLabel: "ins", usage: HID.delete, fnUsage: HID.insert),
                 Key(label: "▦", kind: .trackpad)]

        let row2 = [Key(label: "tab", usage: HID.tab, width: 1.5)] + letters("qwertyuiop")
            + [symbol("[", "{", HID.leftBracket), symbol("]", "}", HID.rightBracket),
               Key(label: "\\", shifted: "|", usage: HID.backslash, width: 1.5)]
        let row3 = [Key(label: "caps", usage: HID.capsLock, width: 1.75)] + letters("asdfghjkl")
            + [symbol(";", ":", HID.semicolon), symbol("'", "\"", HID.quote),
               Key(label: "enter", usage: HID.enter, width: 2.25)]
        // ↑ sits over ↓ (inverted T), so / moves past it, as on compact
        // keyboards with arrows.
        let row4 = [Key(label: "shift", usage: HID.leftShift, width: 2.25, kind: .modifier)] + letters("zxcvbnm")
            + [symbol(",", "<", HID.comma), symbol(".", ">", HID.period),
               Key(label: "shift", usage: HID.rightShift, width: 1.75, kind: .modifier),
               Key(label: "↑", fnLabel: "pgup", usage: HID.up, fnUsage: HID.pageUp),
               symbol("/", "?", HID.slash)]
        let row5 = [
            Key(label: "ctrl", usage: HID.leftControl, width: 1.25, kind: .modifier),
            Key(label: "fn", kind: .fn),
            Key(label: "win", usage: HID.leftGUI, width: 1.25, kind: .modifier),
            Key(label: "alt", usage: HID.leftAlt, width: 1.25, kind: .modifier),
            Key(label: "", usage: HID.space, width: 5),
            Key(label: "alt", usage: HID.rightAlt, width: 1.25, kind: .modifier),
            Key(label: "ctrl", usage: HID.rightControl, kind: .modifier),
            Key(label: "←", fnLabel: "home", usage: HID.left, fnUsage: HID.home),
            Key(label: "↓", fnLabel: "pgdn", usage: HID.down, fnUsage: HID.pageDown),
            Key(label: "→", fnLabel: "end", usage: HID.right, fnUsage: HID.end),
        ]
        return [row0, row1, row2, row3, row4, row5]
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let inset: CGFloat = 6
        let gap: CGFloat = 4
        let area = bounds.insetBy(dx: inset, dy: inset)
        let unitHeight = (area.height - gap * CGFloat(rows.count - 1)) / rowHeights.reduce(0, +)
        var y = area.minY
        frames = rows.enumerated().map { index, row in
            let units = row.reduce(0) { $0 + $1.width }
            let unit = (area.width - gap * CGFloat(row.count - 1)) / units
            let height = unitHeight * rowHeights[index]
            var x = area.minX
            let rects = row.map { key -> CGRect in
                let rect = CGRect(x: x, y: y, width: unit * key.width, height: height)
                x += rect.width + gap
                return rect
            }
            y += height + gap
            return rects
        }
        setNeedsDisplay()
    }

    private var keyboardButton: CGRect {
        CGRect(x: bounds.maxX - 70, y: bounds.minY + 8, width: 62, height: 40)
    }

    // MARK: Drawing

    override func draw(_ rect: CGRect) {
        if trackpadMode {
            drawTrackpad()
            return
        }
        let shift = isShifted
        let fn = fnHeld || fnLatched
        for (r, row) in rows.enumerated() where r < frames.count {
            for (c, key) in row.enumerated() {
                let frame = frames[r][c]
                drawKey(key, in: frame, pressed: isPressed(r, c), lit: isLit(key), shift: shift, fn: fn)
            }
        }
        if momentary {
            let path = UIBezierPath(roundedRect: bounds.insetBy(dx: 4, dy: 4), cornerRadius: 18)
            UIColor(white: 0, alpha: 0.55).setFill()
            path.fill()
            drawCentered("Trackpad", in: bounds, size: 17, color: UIColor(white: 1, alpha: 0.7))
        }
    }

    private func drawKey(_ key: Key, in frame: CGRect, pressed: Bool, lit: Bool, shift: Bool, fn: Bool) {
        let path = UIBezierPath(roundedRect: frame, cornerRadius: min(7, frame.height / 5))
        let fill: UIColor
        if pressed {
            fill = UIColor(white: 1, alpha: 0.34)
        } else if lit {
            fill = tintColor.withAlphaComponent(0.55)
        } else {
            fill = key.kind == .key && key.width == 1 ? UIColor(white: 1, alpha: 0.16) : UIColor(white: 1, alpha: 0.10)
        }
        fill.setFill()
        path.fill()

        let size = min(17, frame.height * 0.42)
        let main: String
        if fn, let label = key.fnLabel {
            main = label
        } else if shift, let shifted = key.shifted {
            main = shifted
        } else {
            main = key.label
        }
        let small = key.label.count > 1 || (fn && key.fnLabel != nil)
        drawCentered(main, in: frame, size: small ? size * 0.72 : size,
                     color: fn && key.fnLabel != nil ? tintColor : .white)
        if !fn, let label = key.fnLabel, frame.height > 30 {
            let corner = CGRect(x: frame.minX + 3, y: frame.maxY - size * 0.62 - 3,
                                width: frame.width - 6, height: size * 0.62)
            draw(label, in: corner, size: size * 0.5, color: tintColor.withAlphaComponent(0.8), alignment: .right)
        }
    }

    private func drawTrackpad() {
        let path = UIBezierPath(roundedRect: bounds.insetBy(dx: 6, dy: 6), cornerRadius: 22)
        UIColor(white: 1, alpha: padTouches.isEmpty ? 0.06 : 0.10).setFill()
        path.fill()
        drawCentered("Drag to move · tap to click · two-finger tap to right-click · two fingers to scroll",
                     in: bounds.insetBy(dx: 40, dy: 0), size: 13, color: UIColor(white: 1, alpha: 0.45))
        let button = UIBezierPath(roundedRect: keyboardButton, cornerRadius: 10)
        UIColor(white: 1, alpha: keyboardButtonTouch == nil ? 0.16 : 0.34).setFill()
        button.fill()
        drawCentered("⌨︎", in: keyboardButton, size: 20, color: .white)
    }

    private func drawCentered(_ text: String, in frame: CGRect, size: CGFloat, color: UIColor) {
        let height = size * 1.3
        draw(text, in: CGRect(x: frame.minX, y: frame.midY - height / 2, width: frame.width, height: height),
             size: size, color: color, alignment: .center)
    }

    private func draw(_ text: String, in frame: CGRect, size: CGFloat, color: UIColor, alignment: NSTextAlignment) {
        let style = NSMutableParagraphStyle()
        style.alignment = alignment
        style.lineBreakMode = .byTruncatingTail
        (text as NSString).draw(in: frame, withAttributes: [
            .font: UIFont.systemFont(ofSize: size, weight: .medium),
            .foregroundColor: color,
            .paragraphStyle: style,
        ])
    }

    private var isShifted: Bool {
        heldModifiers.keys.contains { $0 == HID.leftShift || $0 == HID.rightShift }
            || (keys?.isLatched(HID.leftShift) ?? false) || (keys?.isLatched(HID.rightShift) ?? false)
    }

    private func isPressed(_ row: Int, _ column: Int) -> Bool {
        touched.values.contains { $0.row == row && $0.column == column }
    }

    private func isLit(_ key: Key) -> Bool {
        switch key.kind {
        case .modifier: return keys?.isLatched(key.usage) ?? false
        case .fn: return fnLatched || fnHeld
        case .trackpad: return momentary
        case .key: return false
        }
    }

    // MARK: Touches

    private func hit(_ p: CGPoint) -> (row: Int, column: Int)? {
        for (r, row) in frames.enumerated() {
            for (c, frame) in row.enumerated() where frame.insetBy(dx: -2, dy: -2).contains(p) {
                return (r, c)
            }
        }
        return nil
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        var padBegan: Set<UITouch> = []
        for touch in touches {
            let id = ObjectIdentifier(touch)
            let p = touch.location(in: self)
            if trackpadMode {
                if keyboardButton.contains(p) && keyboardButtonTouch == nil {
                    keyboardButtonTouch = id
                } else {
                    padTouches.insert(id)
                    padBegan.insert(touch)
                }
                continue
            }
            if momentary {
                padTouches.insert(id)
                padBegan.insert(touch)
                continue
            }
            guard let at = hit(p) else { continue }
            press(rows[at.row][at.column], touch: id, at: at, time: touch.timestamp)
        }
        if !padBegan.isEmpty { pad.began(padBegan, in: self) }
        setNeedsDisplay()
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        let moving = touches.filter { padTouches.contains(ObjectIdentifier($0)) }
        if !moving.isEmpty { pad.moved(moving, in: self) }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        lift(touches, cancelled: false)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        lift(touches, cancelled: true)
    }

    private func press(_ key: Key, touch id: ObjectIdentifier, at: (row: Int, column: Int), time: TimeInterval) {
        var sent: UInt16?
        switch key.kind {
        case .trackpad:
            trackpadKey = id
            momentaryTimer?.invalidate()
            momentaryTimer = Timer.scheduledTimer(withTimeInterval: 0.3, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, self.trackpadKey == id else { return }
                    self.momentary = true
                    self.setNeedsDisplay()
                }
            }
        case .fn:
            fnHeld = true
            fnUsed = false
        case .modifier:
            heldModifiers[key.usage] = false
        case .key:
            let fn = fnHeld || fnLatched
            if fn && key.usage == HID.escape {
                onCtrlAltDel()
            } else {
                // Modifiers held by other fingers go down first, for the chord.
                for (usage, down) in heldModifiers where !down {
                    keys?.down(usage)
                    heldModifiers[usage] = true
                }
                let usage = fn ? key.fnUsage ?? key.usage : key.usage
                keys?.down(usage)
                sent = usage
            }
            if fnHeld { fnUsed = true }
        }
        touched[id] = (at.row, at.column, sent)
        tick()
    }

    private func lift(_ touches: Set<UITouch>, cancelled: Bool) {
        var padEnded: Set<UITouch> = []
        for touch in touches {
            let id = ObjectIdentifier(touch)
            if keyboardButtonTouch == id {
                keyboardButtonTouch = nil
                if !cancelled && keyboardButton.contains(touch.location(in: self)) {
                    trackpadMode = false
                    onTrackpadMode(false)
                }
                continue
            }
            if padTouches.remove(id) != nil {
                padEnded.insert(touch)
                continue
            }
            guard let at = touched.removeValue(forKey: id) else { continue }
            release(rows[at.row][at.column], sent: at.usage, time: touch.timestamp, cancelled: cancelled)
        }
        if !padEnded.isEmpty { pad.ended(padEnded, cancelled: cancelled) }
        setNeedsDisplay()
    }

    private func release(_ key: Key, sent: UInt16?, time: TimeInterval, cancelled: Bool) {
        switch key.kind {
        case .trackpad:
            trackpadKey = nil
            momentaryTimer?.invalidate()
            if momentary {
                momentary = false
                pad.reset()
                padTouches.removeAll()
            } else if !cancelled {
                trackpadMode = true
                onTrackpadMode(true)
            }
        case .fn:
            fnHeld = false
            if !fnUsed && !cancelled { fnLatched.toggle() }
        case .modifier:
            guard let down = heldModifiers.removeValue(forKey: key.usage) else { return }
            if down {
                keys?.up(key.usage)
            } else if !cancelled {
                let double = time - (lastModifierTap[key.usage] ?? 0) < 0.35
                keys?.cycle(key.usage, double: double)
                lastModifierTap[key.usage] = time
            }
        case .key:
            // Release what went down, even if Fn changed in between.
            if let sent { keys?.up(sent) }
            if fnLatched && !fnHeld { fnLatched = false }
        }
    }

    private func tick() {
        if haptics { feedback.impactOccurred() }
    }

    private func releaseKeys() {
        for case let usage? in touched.values.map(\.usage) { keys?.up(usage) }
        for (usage, down) in heldModifiers where down { keys?.up(usage) }
        touched.removeAll()
        heldModifiers.removeAll()
        fnHeld = false
        momentary = false
        trackpadKey = nil
    }

    /// The deck goes away or focus leaves: release everything.
    func releaseAll() {
        releaseKeys()
        pad.reset()
        padTouches.removeAll()
    }
}
