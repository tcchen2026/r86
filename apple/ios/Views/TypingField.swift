import SwiftUI
import UIKit

/// The system keyboard for the guest: a hidden view that takes text. Typed
/// characters become US-layout key presses; backspace and return become
/// their keys. Hardware keys pressed while it is up go straight through.
/// `accessory` (the key row) rides on top of the keyboard in every layout.
struct TypingField<Accessory: View>: UIViewRepresentable {
    @Binding var isActive: Bool
    let keys: KeyInput
    @ViewBuilder let accessory: Accessory

    final class Coordinator {
        var host: UIHostingController<Accessory>?
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> TypingUIView {
        let view = TypingUIView()
        let host = UIHostingController(rootView: accessory)
        host.view.backgroundColor = .clear
        let bar = UIView(frame: CGRect(x: 0, y: 0, width: 0, height: 54))
        host.view.frame = bar.bounds
        host.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        bar.addSubview(host.view)
        view.accessory = bar
        context.coordinator.host = host
        return view
    }

    func updateUIView(_ view: TypingUIView, context: Context) {
        context.coordinator.host?.rootView = accessory
        view.keys = keys
        let binding = $isActive
        view.onDismiss = {
            DispatchQueue.main.async {
                if binding.wrappedValue { binding.wrappedValue = false }
            }
        }
        if isActive && !view.isFirstResponder {
            DispatchQueue.main.async { view.becomeFirstResponder() }
        } else if !isActive && view.isFirstResponder {
            DispatchQueue.main.async { _ = view.resignFirstResponder() }
        }
    }
}

final class TypingUIView: UIView, UIKeyInput {
    var keys: KeyInput?
    var onDismiss: () -> Void = {}
    var accessory: UIView?
    private var held: Set<UInt16> = []

    override var inputAccessoryView: UIView? { accessory }

    // Plain keys: no corrections, capitals, smart quotes or dashes.
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var autocorrectionType: UITextAutocorrectionType = .no
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .asciiCapable
    var keyboardAppearance: UIKeyboardAppearance = .dark

    override var canBecomeFirstResponder: Bool { true }

    /// Always true, so backspace reaches deleteBackward.
    var hasText: Bool { true }

    func insertText(_ text: String) {
        keys?.type(text)
    }

    func deleteBackward() {
        keys?.tap(HID.backspace)
    }

    override func resignFirstResponder() -> Bool {
        for usage in held { keys?.up(usage) }
        held.removeAll()
        let resigned = super.resignFirstResponder()
        if resigned { onDismiss() }
        return resigned
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let keyPresses = presses.filter { $0.key != nil }
        for press in keyPresses {
            guard let code = press.key?.keyCode.rawValue else { continue }
            let usage = UInt16(truncatingIfNeeded: code)
            held.insert(usage)
            keys?.down(usage)
        }
        let rest = presses.subtracting(keyPresses)
        if !rest.isEmpty { super.pressesBegan(rest, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let keyPresses = presses.filter { $0.key != nil }
        for press in keyPresses {
            guard let code = press.key?.keyCode.rawValue else { continue }
            let usage = UInt16(truncatingIfNeeded: code)
            if held.remove(usage) != nil { keys?.up(usage) }
        }
        let rest = presses.subtracting(keyPresses)
        if !rest.isEmpty { super.pressesEnded(rest, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        pressesEnded(presses, with: event)
    }
}
