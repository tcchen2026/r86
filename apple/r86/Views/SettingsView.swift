import AppKit
import SwiftUI

/// The Settings window.
struct SettingsView: View {
    var body: some View {
        TabView {
            Form { GeneralSettingsSection() }
                .formStyle(.grouped)
                .tabItem { Label("General", systemImage: "gearshape") }
            Form { DisplaySettingsSection() }
                .formStyle(.grouped)
                .tabItem { Label("Display", systemImage: "display") }
            Form { InputSettingsSection() }
                .formStyle(.grouped)
                .tabItem { Label("Input", systemImage: "keyboard") }
            Form { AboutSection() }
                .formStyle(.grouped)
                .tabItem { Label("About", systemImage: "info.circle") }
        }
        .frame(width: 500, height: 340)
    }
}

struct GeneralSettingsSection: View {
    @Environment(Preferences.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        Section {
            Toggle("Ask before shutting down", isOn: $prefs.confirmStop)
            LabeledContent("Machines folder") {
                Button("Show in Finder") {
                    NSWorkspace.shared.activateFileViewerSelecting([R86.library])
                }
            }
        } header: {
            Text("General")
        } footer: {
            Text("Machines use the same folders and vm.toml as r86 on handhelds, so they can be copied between devices.")
        }
    }
}

struct DisplaySettingsSection: View {
    @Environment(Preferences.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        Section {
            Picker("Scaling", selection: $prefs.scaling) {
                ForEach(DisplayScaling.allCases) { s in
                    Text(s.title).tag(s)
                }
            }
            Toggle("Smooth scaling", isOn: $prefs.smoothScaling)
            Toggle("Speed overlay", isOn: $prefs.showSpeed)
        } header: {
            Text("Display")
        } footer: {
            Text("Keep Aspect shows 320×200, 640×400 and text modes at 4:3, as on a CRT.")
        }
    }
}

struct InputSettingsSection: View {
    @Environment(Preferences.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        Section {
            Toggle("Send ⌘ to the guest as the Windows key", isOn: $prefs.sendCommandKeys)
            LabeledContent("Capture the pointer", value: "Click the screen")
            LabeledContent("Release the pointer", value: "⌃ ⌥")
        } header: {
            Text("Input")
        } footer: {
            Text("When off, ⌘ shortcuts such as ⌘Q and ⌘W stay with r86 and the guest never sees ⌘.")
        }
    }
}

struct AboutSection: View {
    var body: some View {
        Section {
            LabeledContent("Version",
                           value: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
            LabeledContent("Guest", value: "32-bit x86 PC (interpreter)")
            LabeledContent("License", value: "Apache-2.0")
        } header: {
            Text("About r86")
        }
    }
}
