import AppKit
import QuartzCore
import SwiftUI

/// The guest screen in SwiftUI. Keyboard and mouse need AppKit events:
/// physical key codes, key-up and modifier changes, and relative motion.
struct GuestScreen: NSViewRepresentable {
    let session: Session
    let scaling: DisplayScaling
    let smooth: Bool
    let sendCommandKeys: Bool
    var onCapture: (Bool) -> Void = { _ in }

    func makeNSView(context: Context) -> GuestNSView {
        let view = GuestNSView()
        update(view)
        return view
    }

    func updateNSView(_ view: GuestNSView, context: Context) {
        update(view)
    }

    static func dismantleNSView(_ view: GuestNSView, coordinator: Void) {
        view.detach()
    }

    private func update(_ view: GuestNSView) {
        view.session = session
        view.scaling = scaling
        view.smooth = smooth
        view.sendCommandKeys = sendCommandKeys
        view.onCapture = onCapture
    }
}

final class GuestNSView: NSView {
    var session: Session?
    var scaling: DisplayScaling = .aspect { didSet { needsLayout = true } }
    var smooth = false { didSet { applyFilter() } }
    var sendCommandKeys = false
    var onCapture: (Bool) -> Void = { _ in }

    private let screen = CALayer()
    private var link: CADisplayLink?
    private var imageSize = CGSize.zero
    private var captured = false
    private var buttons: UInt32 = 0
    private var heldModifiers = Set<UInt16>()
    private var motion = CGPoint.zero
    private var resignObserver: NSObjectProtocol?

    private static let commandKeys: Set<UInt16> = [0x36, 0x37]
    private static let capsLock: UInt16 = 0x39

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        screen.contentsGravity = .resize
        layer?.addSublayer(screen)
        applyFilter()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var acceptsFirstResponder: Bool { true }
    override var isFlipped: Bool { true }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        link?.invalidate()
        link = nil
        if let resignObserver { NotificationCenter.default.removeObserver(resignObserver) }
        resignObserver = nil
        guard let window else { return }
        let link = displayLink(target: self, selector: #selector(step(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
        resignObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didResignKeyNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.releaseInput() }
        }
        window.makeFirstResponder(self)
    }

    /// The window closes: stop drawing and give the pointer back.
    func detach() {
        releaseInput()
        link?.invalidate()
        link = nil
        if let resignObserver { NotificationCenter.default.removeObserver(resignObserver) }
        resignObserver = nil
        session = nil
    }

    @objc private func step(_ link: CADisplayLink) {
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

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layoutScreen()
        CATransaction.commit()
    }

    private func applyFilter() {
        screen.magnificationFilter = smooth ? .linear : .nearest
        screen.minificationFilter = .linear
    }

    private func layoutScreen() {
        let bounds = self.bounds
        guard imageSize.width > 0, imageSize.height > 0, bounds.width > 0, bounds.height > 0 else {
            screen.frame = bounds
            return
        }
        var size: CGSize
        switch scaling {
        case .stretch:
            size = bounds.size
        case .integer:
            let backing = window?.backingScaleFactor ?? 2
            let k = max(1, floor(min(bounds.width * backing / imageSize.width,
                                     bounds.height * backing / imageSize.height)))
            size = CGSize(width: imageSize.width * k / backing, height: imageSize.height * k / backing)
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

    // MARK: Keyboard

    override func keyDown(with event: NSEvent) {
        forward(event.keyCode, down: true)
    }

    override func keyUp(with event: NSEvent) {
        forward(event.keyCode, down: false)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        // With ⌘ passed through, shortcuts reach the guest instead of the menus.
        guard sendCommandKeys, window?.firstResponder === self,
              event.modifierFlags.contains(.command) else { return super.performKeyEquivalent(with: event) }
        forward(event.keyCode, down: true)
        return true
    }

    override func flagsChanged(with event: NSEvent) {
        let code = event.keyCode
        if code == Self.capsLock {
            // macOS reports Caps Lock as a toggle; the guest gets a keystroke.
            forward(code, down: true)
            forward(code, down: false)
        } else if heldModifiers.remove(code) != nil {
            forward(code, down: false)
        } else {
            heldModifiers.insert(code)
            forward(code, down: true)
        }
        if captured && event.modifierFlags.contains([.control, .option]) {
            endCapture()
        }
    }

    private func forward(_ code: UInt16, down: Bool) {
        if Self.commandKeys.contains(code) && !sendCommandKeys { return }
        session?.key(code, down: down)
    }

    // MARK: Mouse

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        guard captured else { return beginCapture() }
        press(1, true)
    }

    override func mouseUp(with event: NSEvent) { press(1, false) }
    override func rightMouseDown(with event: NSEvent) { if captured { press(2, true) } }
    override func rightMouseUp(with event: NSEvent) { press(2, false) }
    override func otherMouseDown(with event: NSEvent) { if captured { press(4, true) } }
    override func otherMouseUp(with event: NSEvent) { press(4, false) }
    override func mouseMoved(with event: NSEvent) { move(event) }
    override func mouseDragged(with event: NSEvent) { move(event) }
    override func rightMouseDragged(with event: NSEvent) { move(event) }
    override func otherMouseDragged(with event: NSEvent) { move(event) }

    private func press(_ bit: UInt32, _ down: Bool) {
        guard captured || !down else { return }
        let before = buttons
        buttons = down ? buttons | bit : buttons & ~bit
        if buttons != before { session?.mouse(dx: 0, dy: 0, buttons: buttons) }
    }

    private func move(_ event: NSEvent) {
        guard captured else { return }
        // Keep fractions of a pixel for the next event.
        motion.x += event.deltaX
        motion.y += event.deltaY
        let dx = motion.x.rounded(.towardZero)
        let dy = motion.y.rounded(.towardZero)
        motion.x -= dx
        motion.y -= dy
        if dx != 0 || dy != 0 {
            session?.mouse(dx: Int32(dx), dy: Int32(dy), buttons: buttons)
        }
    }

    private func beginCapture() {
        guard !captured, session != nil else { return }
        captured = true
        window?.acceptsMouseMovedEvents = true
        NSCursor.hide()
        _ = CGAssociateMouseAndMouseCursorPosition(0)
        onCapture(true)
    }

    private func endCapture() {
        guard captured else { return }
        captured = false
        buttons = 0
        motion = .zero
        _ = CGAssociateMouseAndMouseCursorPosition(1)
        NSCursor.unhide()
        onCapture(false)
    }

    /// Focus left the window: release the pointer and everything held.
    private func releaseInput() {
        endCapture()
        heldModifiers.removeAll()
        session?.send(.releaseInput)
    }
}
