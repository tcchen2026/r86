import SwiftUI

/// The iPhone Duo folded like a laptop, its fold across the screen: the
/// guest screen above the fold, the deck (keyboard or trackpad) below it.
/// Nothing sits on the fold band iOS reserves.
struct LaptopConsole: View {
    @Environment(Library.self) private var library
    @Environment(Preferences.self) private var prefs
    @Environment(Stage.self) private var stage

    let machine: Machine
    /// The fold band, in this view's coordinates.
    let fold: CGRect
    let size: CGSize

    private var running: Bool { machine.status == .running }

    var body: some View {
        ZStack(alignment: .topLeading) {
            Color.black

            upper
                .frame(width: size.width, height: max(0, fold.minY))

            lower
                .frame(width: size.width, height: max(0, size.height - fold.maxY))
                .offset(y: fold.maxY)
        }
        .frame(width: size.width, height: size.height, alignment: .topLeading)
    }

    private var upper: some View {
        ZStack(alignment: .top) {
            Color.black
            if let session = library.session(machine.id) {
                // A touchscreen laptop: with r86 tools, taps click where you touch.
                GuestScreen(session: session, keys: stage.keys, scaling: prefs.scaling,
                            smooth: prefs.smoothScaling,
                            touches: running ? .touch : nil,
                            takesKeys: stage.sheet == nil,
                            speed: stage.pointerSpeed, haptics: stage.haptics)
                    .opacity(machine.status == .paused ? 0.5 : 1)
            }
            PowerOverlay(machine: machine)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            FloatingCommandBar(machine: machine, compact: true)
                .padding(.top, 8)
        }
        .overlay(alignment: .topTrailing) {
            if prefs.showSpeed, running {
                SpeedOverlay(machine: machine).padding(.top, 60).padding(.trailing, 12)
            }
        }
    }

    @ViewBuilder
    private var lower: some View {
        @Bindable var stage = stage
        if running, let session = library.session(machine.id) {
            DeckView(session: session, keys: stage.keys, trackpadMode: $stage.deckTrackpad,
                     speed: stage.pointerSpeed, haptics: stage.haptics,
                     onCtrlAltDel: { library.ctrlAltDel(machine.id) })
        } else if machine.status == .stopped {
            OffDetails(machine: machine)
                .padding(10)
        } else {
            Color.black
        }
    }
}
