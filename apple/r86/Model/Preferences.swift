import Observation
import SwiftUI

enum DisplayScaling: String, CaseIterable, Identifiable {
    /// Fit the window; 320x200, 640x400 and 720x400 shown at 4:3 as on a CRT.
    case aspect
    case stretch
    /// The largest whole multiple of the guest's pixels that fits.
    case integer

    var id: String { rawValue }

    var title: String {
        switch self {
        case .aspect: return "Keep Aspect"
        case .stretch: return "Stretch"
        case .integer: return "Integer Scale"
        }
    }
}

/// Why a machine left fast mode for the compatible engine.
enum FastNotice: String, Identifiable {
    /// Lockdown Mode stops WebKit from compiling code (iOS).
    case lockdown
    case unavailable

    var id: String { rawValue }

    var message: String {
        switch self {
        case .lockdown:
            return "Lockdown Mode is on, so this machine runs in the slower compatible mode.\n\n"
                + "To use fast mode and keep Lockdown Mode on for everything else, open Settings › "
                + "Privacy & Security › Lockdown Mode › Configure Web Browsing and turn off r86."
        case .unavailable:
            return "This device cannot run fast mode. The machine runs in the compatible mode."
        }
    }
}

/// App settings in UserDefaults. Each property reports reads and writes to
/// Observation by hand, so views update when a setting changes.
@MainActor
@Observable
final class Preferences {
    static let shared = Preferences()

    nonisolated private var store: UserDefaults { .standard }

    nonisolated private func bool(_ key: String, _ fallback: Bool) -> Bool {
        store.object(forKey: key) == nil ? fallback : store.bool(forKey: key)
    }

    var scaling: DisplayScaling {
        get { access(keyPath: \.scaling); return DisplayScaling(rawValue: store.string(forKey: "scaling") ?? "") ?? .aspect }
        set { withMutation(keyPath: \.scaling) { store.set(newValue.rawValue, forKey: "scaling") } }
    }

    /// Linear filtering when the guest screen is scaled; off keeps pixels sharp.
    var smoothScaling: Bool {
        get { access(keyPath: \.smoothScaling); return bool("smoothScaling", false) }
        set { withMutation(keyPath: \.smoothScaling) { store.set(newValue, forKey: "smoothScaling") } }
    }

    var showSpeed: Bool {
        get { access(keyPath: \.showSpeed); return bool("showSpeed", false) }
        set { withMutation(keyPath: \.showSpeed) { store.set(newValue, forKey: "showSpeed") } }
    }

    /// Pass ⌘ (as the Windows key) and ⌘ shortcuts to the guest.
    var sendCommandKeys: Bool {
        get { access(keyPath: \.sendCommandKeys); return bool("sendCommandKeys", false) }
        set { withMutation(keyPath: \.sendCommandKeys) { store.set(newValue, forKey: "sendCommandKeys") } }
    }

    /// iOS: run machines that suit it in a web view, where WebKit compiles
    /// r86's Wasm JIT (ios/Platform/WebEngine.swift).
    var fastMode: Bool {
        get { access(keyPath: \.fastMode); return bool("fastMode", false) }
        set { withMutation(keyPath: \.fastMode) { store.set(newValue, forKey: "fastMode") } }
    }

    /// Do not explain again why a machine left fast mode.
    var fastNoticeHidden: Bool {
        get { access(keyPath: \.fastNoticeHidden); return bool("fastNoticeHidden", false) }
        set { withMutation(keyPath: \.fastNoticeHidden) { store.set(newValue, forKey: "fastNoticeHidden") } }
    }

    /// Ask before shutting down from the toolbar.
    var confirmStop: Bool {
        get { access(keyPath: \.confirmStop); return bool("confirmStop", true) }
        set { withMutation(keyPath: \.confirmStop) { store.set(newValue, forKey: "confirmStop") } }
    }
}
