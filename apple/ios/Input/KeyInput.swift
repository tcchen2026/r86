import Foundation
import Observation

/// Sends keys to the machine on screen for every keyboard: hardware keys,
/// the key row, the laptop deck and typed text.
///
/// A modifier tapped on screen stays down for the next key (sticky) and is
/// released after it; a double tap locks it until tapped again. Held keys
/// repeat: r86's keyboard sends what the host sends, and iOS repeats neither
/// hardware nor on-screen keys.
@MainActor
@Observable
final class KeyInput {
    @ObservationIgnored weak var session: Session?

    /// Modifiers down for the next key only.
    private(set) var sticky: Set<UInt16> = []
    /// Modifiers down until tapped again.
    private(set) var locked: Set<UInt16> = []

    @ObservationIgnored private var repeatTimer: Timer?
    @ObservationIgnored private var repeating: UInt16?

    /// Press and release a key, with Shift when asked and the sticky
    /// modifiers, which it then releases.
    func tap(_ usage: UInt16, shift: Bool = false) {
        guard let session else { return }
        if shift { session.hidKey(HID.leftShift, down: true) }
        session.hidKey(usage, down: true)
        session.hidKey(usage, down: false)
        if shift { session.hidKey(HID.leftShift, down: false) }
        releaseSticky()
    }

    /// Text typed on the system keyboard, as US-layout keys.
    func type(_ text: String) {
        for character in text {
            if let key = HID.key(for: character) { tap(key.usage, shift: key.shift) }
        }
    }

    /// Hold a key until `up`. Keys other than modifiers repeat after a
    /// short delay, at 30 a second.
    func down(_ usage: UInt16) {
        session?.hidKey(usage, down: true)
        if !HID.isModifier(usage) { startRepeat(usage) }
    }

    func up(_ usage: UInt16) {
        if repeating == usage { stopRepeat() }
        session?.hidKey(usage, down: false)
        if !HID.isModifier(usage) { releaseSticky() }
    }

    func isLatched(_ usage: UInt16) -> Bool { sticky.contains(usage) || locked.contains(usage) }

    /// A tap on an on-screen modifier: up → sticky → locked when tapped again
    /// quickly (`double`) → up.
    func cycle(_ usage: UInt16, double: Bool) {
        if locked.remove(usage) != nil {
            session?.hidKey(usage, down: false)
        } else if sticky.contains(usage) {
            sticky.remove(usage)
            if double {
                locked.insert(usage)
            } else {
                session?.hidKey(usage, down: false)
            }
        } else {
            sticky.insert(usage)
            session?.hidKey(usage, down: true)
        }
    }

    /// Release the sticky modifiers; locked ones stay down.
    func releaseSticky() {
        guard !sticky.isEmpty else { return }
        for usage in sticky { session?.hidKey(usage, down: false) }
        sticky.removeAll()
    }

    /// Everything up: focus left, or the machine on screen changed.
    func releaseAll() {
        stopRepeat()
        releaseSticky()
        for usage in locked { session?.hidKey(usage, down: false) }
        locked.removeAll()
    }

    private func startRepeat(_ usage: UInt16) {
        stopRepeat()
        repeating = usage
        repeatTimer = Timer.scheduledTimer(withTimeInterval: 0.45, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.repeatSoon(usage) }
        }
    }

    private func repeatSoon(_ usage: UInt16) {
        guard repeating == usage else { return }
        repeatTimer = Timer.scheduledTimer(withTimeInterval: 1.0 / 30, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.repeating == usage else { return }
                self.session?.hidKey(usage, down: true)
            }
        }
    }

    private func stopRepeat() {
        repeatTimer?.invalidate()
        repeatTimer = nil
        repeating = nil
    }
}
