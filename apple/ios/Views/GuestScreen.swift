import QuartzCore
import SwiftUI
import UIKit

/// The guest screen: the latest frame, the hardware keyboard and touches.
struct GuestScreen: UIViewRepresentable {
    let session: Session
    let keys: KeyInput
    let scaling: DisplayScaling
    let smooth: Bool
    /// How touches on the screen point; nil shows the screen only.
    var touches: InputMode?
    /// Touch mode without the guest's r86 tools helper: the full-screen
    /// layout falls back to the trackpad, the others ignore touches.
    var fallback: InputMode?
    /// Take the hardware keyboard (first responder) while nothing else does.
    var takesKeys = true
    var speed: CGFloat = 1
    var haptics = true

    func makeUIView(context: Context) -> GuestUIView {
        let view = GuestUIView()
        update(view)
        return view
    }

    func updateUIView(_ view: GuestUIView, context: Context) {
        update(view)
        if takesKeys && !view.isFirstResponder && view.window != nil {
            DispatchQueue.main.async { view.becomeFirstResponder() }
        }
    }

    static func dismantleUIView(_ view: GuestUIView, coordinator: ()) {
        view.detach()
    }

    private func update(_ view: GuestUIView) {
        keys.session = session
        view.session = session
        view.keys = keys
        view.scaling = scaling
        view.smooth = smooth
        view.touchMode = touches
        view.fallback = fallback
        view.takesKeys = takesKeys
        view.trackpad.speed = speed
        view.haptics = haptics
    }
}

final class GuestUIView: UIView {
    var session: Session? {
        didSet {
            trackpad.session = session
            if session !== oldValue { attachWeb() }
        }
    }
    var keys: KeyInput?
    var scaling: DisplayScaling = .aspect { didSet { setNeedsLayout() } }
    var smooth = false { didSet { applyFilter() } }
    var touchMode: InputMode?
    var fallback: InputMode?
    var takesKeys = true
    var haptics = true
    let trackpad = TrackpadInterpreter()

    private let screen = CALayer()
    /// Fast mode: the page draws the guest. Its web view sits where `screen`
    /// would be, under this view's touches.
    private weak var engine: WebEngine?
    /// The page's view while this view shows it.
    private var page: UIView? {
        guard let view = engine?.view, view.superview === self else { return nil }
        return view
    }
    private var link: CADisplayLink?
    private var imageSize = CGSize.zero
    private let feedback = UIImpactFeedbackGenerator(style: .light)
    /// Hardware keys held, released if focus leaves.
    private var held: Set<UInt16> = []

    /// The mode of the touch sequence in progress.
    private var mode: InputMode?
    /// Direct touch: one finger points; a quick tap clicks, moving drags,
    /// holding still right-clicks.
    private var finger: UITouch?
    /// Where the finger landed, and where its click goes (the last tap's
    /// spot for a double tap).
    private var down = CGPoint.zero
    private var start = CGPoint.zero
    /// The last tap, for double-clicks.
    private var lastTap: (time: TimeInterval, at: CGPoint)?
    private var dragging = false
    private var heldDown = false
    private var holdTimer: Timer?
    /// A mouse or trackpad on iPad: buttons from the event.
    private var pointerButtons: UInt32 = 0
    private var last = (x: UInt16(32768), y: UInt16(32768))
    private var scrolled: CGFloat = 0

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        isMultipleTouchEnabled = true
        screen.contentsGravity = .resize
        layer.addSublayer(screen)
        applyFilter()
        addGestureRecognizer(UIHoverGestureRecognizer(target: self, action: #selector(hover(_:))))
        let wheel = UIPanGestureRecognizer(target: self, action: #selector(self.scroll(_:)))
        wheel.allowedScrollTypesMask = .all
        wheel.allowedTouchTypes = []
        addGestureRecognizer(wheel)
        trackpad.onClick = { [weak self] in self?.tick() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var canBecomeFirstResponder: Bool { true }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        link?.invalidate()
        link = nil
        guard window != nil else { return }
        let link = CADisplayLink(target: self, selector: #selector(step(_:)))
        // r86 renders at most 60 frames a second.
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 60, preferred: 60)
        link.add(to: .main, forMode: .common)
        self.link = link
        if takesKeys { becomeFirstResponder() }
    }

    /// The view goes away: stop drawing and release everything held.
    func detach() {
        releaseInput()
        link?.invalidate()
        link = nil
        session = nil
    }

    private func attachWeb() {
        // Leaving: unless another GuestUIView already took the page (a layout
        // change), it waits in the window, still running.
        if let engine, engine.view.superview === self { engine.park() }
        engine = session?.web
        screen.isHidden = engine != nil
        guard let engine else { return }
        imageSize = .zero
        insertSubview(engine.view, at: 0)
        engine.setSmooth(smooth)
        setNeedsLayout()
    }

    override func resignFirstResponder() -> Bool {
        releaseKeys()
        return super.resignFirstResponder()
    }

    @objc private func step(_ link: CADisplayLink) {
        if page != nil, let size = session?.webSize() {
            if size != imageSize {
                imageSize = size
                setNeedsLayout()
            }
            return
        }
        guard let image = session?.newImage() else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        screen.contents = image
        let size = CGSize(width: image.width, height: image.height)
        if size != imageSize {
            imageSize = size
            layoutScreen()
        }
        CATransaction.commit()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layoutScreen()
        CATransaction.commit()
    }

    private func applyFilter() {
        screen.magnificationFilter = smooth ? .linear : .nearest
        screen.minificationFilter = .linear
        session?.web?.setSmooth(smooth)
    }

    private func layoutScreen() {
        defer { page?.frame = screen.frame }
        guard imageSize.width > 0, imageSize.height > 0, bounds.width > 0, bounds.height > 0 else {
            screen.frame = bounds
            return
        }
        var size: CGSize
        switch scaling {
        case .stretch:
            size = bounds.size
        case .integer:
            let scale = traitCollection.displayScale
            let k = max(1, floor(min(bounds.width * scale / imageSize.width,
                                     bounds.height * scale / imageSize.height)))
            size = CGSize(width: imageSize.width * k / scale, height: imageSize.height * k / scale)
        case .aspect:
            // CRT modes with non-square pixels fill a 4:3 screen.
            let crt = imageSize.height == 200 || imageSize.height == 400
            let aspect = crt ? 4.0 / 3.0 : imageSize.width / imageSize.height
            size = bounds.width / bounds.height > aspect
                ? CGSize(width: bounds.height * aspect, height: bounds.height)
                : CGSize(width: bounds.width, height: bounds.width / aspect)
        }
        screen.frame = CGRect(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2,
                              width: size.width, height: size.height)
    }

    // MARK: Hardware keyboard

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let keyPresses = presses.filter { $0.key != nil }
        for press in keyPresses {
            guard let code = press.key?.keyCode.rawValue else { continue }
            let usage = UInt16(truncatingIfNeeded: code)
            held.insert(usage)
            keys?.down(usage)
        }
        if keyPresses.count < presses.count {
            super.pressesBegan(presses.subtracting(keyPresses), with: event)
        }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        release(presses, with: event, cancelled: false)
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        release(presses, with: event, cancelled: true)
    }

    private func release(_ presses: Set<UIPress>, with event: UIPressesEvent?, cancelled: Bool) {
        let keyPresses = presses.filter { $0.key != nil }
        for press in keyPresses {
            guard let code = press.key?.keyCode.rawValue else { continue }
            let usage = UInt16(truncatingIfNeeded: code)
            if held.remove(usage) != nil { keys?.up(usage) }
        }
        let rest = presses.subtracting(keyPresses)
        guard !rest.isEmpty else { return }
        if cancelled {
            super.pressesCancelled(rest, with: event)
        } else {
            super.pressesEnded(rest, with: event)
        }
    }

    private func releaseKeys() {
        for usage in held { keys?.up(usage) }
        held.removeAll()
    }

    // MARK: Touches

    private var pointerActive: Bool { session?.pointerActive ?? false }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        if takesKeys && !isFirstResponder { becomeFirstResponder() }
        if mode == nil {
            let wanted = touchMode == .touch && !pointerActive ? fallback : touchMode
            mode = touches.first?.type == .indirectPointer && pointerActive ? .touch : wanted
        }
        switch mode {
        case .trackpad: trackpad.began(touches, in: self)
        case .touch: directBegan(touches, event)
        case nil: break
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        switch mode {
        case .trackpad: trackpad.moved(touches, in: self)
        case .touch: directMoved(touches, event)
        case nil: break
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish(touches, event, cancelled: false)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish(touches, event, cancelled: true)
    }

    private func finish(_ touches: Set<UITouch>, _ event: UIEvent?, cancelled: Bool) {
        switch mode {
        case .trackpad: trackpad.ended(touches, cancelled: cancelled)
        case .touch: directEnded(touches, cancelled: cancelled)
        case nil: break
        }
        let remaining = event?.allTouches?.filter { $0.phase != .ended && $0.phase != .cancelled } ?? []
        if remaining.isEmpty { mode = nil }
    }

    private func directBegan(_ touches: Set<UITouch>, _ event: UIEvent?) {
        guard finger == nil, let touch = touches.first else { return }
        finger = touch
        down = touch.location(in: self)
        start = down
        if touch.type == .indirectPointer {
            // A mouse or trackpad click: the buttons come with the event.
            let mask = event?.buttonMask ?? .primary
            pointerButtons = (mask.contains(.secondary) ? 2 : 0) | (mask.contains(.primary) || mask.isEmpty ? 1 : 0)
            point(start, buttons: pointerButtons)
            return
        }
        dragging = false
        heldDown = false
        // A second tap soon after the last and near it lands on the same
        // spot: Windows' double-click area is a few pixels, fingers land
        // farther apart.
        if let tap = lastTap, touch.timestamp - tap.time < 0.5,
           hypot(start.x - tap.at.x, start.y - tap.at.y) < 24 {
            start = tap.at
        }
        lastTap = nil
        point(start, buttons: 0)
        holdTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.longPress() }
        }
    }

    private func directMoved(_ touches: Set<UITouch>, _ event: UIEvent?) {
        guard let finger, touches.contains(finger) else { return }
        let p = finger.location(in: self)
        if finger.type == .indirectPointer {
            point(p, buttons: pointerButtons)
            return
        }
        if !dragging && !heldDown && hypot(p.x - down.x, p.y - down.y) > 8 {
            holdTimer?.invalidate()
            dragging = true
            point(start, buttons: 1)
        }
        if dragging { point(p, buttons: 1) }
    }

    private func directEnded(_ touches: Set<UITouch>, cancelled: Bool) {
        guard let finger, touches.contains(finger) else { return }
        let p = finger.location(in: self)
        holdTimer?.invalidate()
        if finger.type == .indirectPointer {
            pointerButtons = 0
            point(p, buttons: 0)
        } else if dragging {
            point(p, buttons: 0)
        } else if !heldDown && !cancelled {
            point(start, buttons: 1)
            point(start, buttons: 0)
            lastTap = (finger.timestamp, start)
            tick()
        }
        self.finger = nil
        dragging = false
        heldDown = false
    }

    /// Holding still for half a second right-clicks.
    private func longPress() {
        guard finger != nil, !dragging else { return }
        heldDown = true
        point(start, buttons: 2)
        point(start, buttons: 0)
        tick()
    }

    @objc private func hover(_ recognizer: UIHoverGestureRecognizer) {
        guard pointerActive, finger == nil else { return }
        point(recognizer.location(in: self), buttons: 0)
    }

    /// Two-finger scrolling on an iPad trackpad or a mouse wheel.
    @objc private func scroll(_ recognizer: UIPanGestureRecognizer) {
        guard pointerActive else { return }
        if recognizer.state == .began { scrolled = 0 }
        let y = recognizer.translation(in: self).y
        let steps = ((y - scrolled) / 12).rounded(.towardZero)
        guard steps != 0 else { return }
        scrolled += steps * 12
        session?.pointer(x: last.x, y: last.y, buttons: pointerButtons, wheel: Int16(max(-127, min(127, -steps))))
    }

    /// Absolute position on the drawn screen, clamped, for the r86 tools helper.
    private func point(_ p: CGPoint, buttons: UInt32) {
        let frame = screen.frame
        guard frame.width > 1, frame.height > 1 else { return }
        func scale(_ v: CGFloat, _ origin: CGFloat, _ size: CGFloat) -> UInt16 {
            UInt16(max(0, min(1, (v - origin) / (size - 1))) * 65535)
        }
        last = (scale(p.x, frame.minX, frame.width), scale(p.y, frame.minY, frame.height))
        session?.pointer(x: last.x, y: last.y, buttons: buttons, wheel: 0)
    }

    private func tick() {
        if haptics { feedback.impactOccurred() }
    }

    /// Focus left: release keys, buttons and any touch in progress.
    func releaseInput() {
        releaseKeys()
        holdTimer?.invalidate()
        if dragging || pointerButtons != 0 {
            session?.pointer(x: last.x, y: last.y, buttons: 0, wheel: 0)
        }
        finger = nil
        dragging = false
        pointerButtons = 0
        mode = nil
        trackpad.reset()
    }
}
