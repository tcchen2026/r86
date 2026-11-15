import SwiftUI

/// Settings on iPhone and iPad, in one form.
struct SettingsSheet: View {
    @Environment(Preferences.self) private var prefs
    @Environment(Stage.self) private var stage
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        @Bindable var prefs = prefs
        @Bindable var stage = stage
        NavigationStack {
            Form {
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
                    Text("Keep Aspect shows 320x200, 640x400 and 720x400 at 4:3, as on a CRT.")
                }

                Section {
                    Picker("Pointing", selection: $stage.inputMode) {
                        ForEach(InputMode.allCases) { mode in
                            Label(mode.title, systemImage: mode.symbol).tag(mode)
                        }
                    }
                    LabeledContent("Trackpad speed") {
                        Slider(value: $stage.pointerSpeed, in: 0.5...2)
                            .frame(maxWidth: 180)
                    }
                    Toggle("Haptics", isOn: $stage.haptics)
                } header: {
                    Text("Input")
                } footer: {
                    Text("Touch clicks where you tap; it needs r86 Tools in the guest (More › CD › Install r86 Tools). On an iPhone Duo folded like a laptop, the lower half is the keyboard; ▦ turns it into a trackpad.")
                }

                Section {
                    Toggle("Ask before turning off", isOn: $prefs.confirmStop)
                } header: {
                    Text("Machines")
                } footer: {
                    Text("Machines are kept in Files › r86 › VMs, in the same folders and vm.toml as r86 on handhelds and the Mac, so they can be copied between devices. A machine pauses when r86 goes to the background.")
                }

                Section("About") {
                    LabeledContent("Version", value: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
                    LabeledContent("License", value: "Apache-2.0")
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }
}
