import SwiftUI

/// The iPhone Duo's laptop posture: partly folded with the fold across the
/// screen. iOS reports the fold as a reserved division region, active only
/// while the phone is folded; Apple advises laying out from it rather than
/// from the hinge angle. Its band is read at run time, never hard-coded.
///
/// The APIs need the iOS 27.1 SDK. script/build_ios.sh defines R86_DUO when
/// the SDK has them; other builds, and phones without a hinge, never see a
/// laptop posture.
enum Fold {
    /// The active horizontal fold band in the proxy's coordinates, or nil
    /// when flat, closed, folded like a book, or on a phone without a hinge.
    @MainActor
    static func laptop(in proxy: GeometryProxy) -> CGRect? {
        #if R86_DUO
        if #available(iOS 27.1, *) {
            return proxy.reservedRegions(kind: .division)
                .first { $0.isActive && $0.frame.width > $0.frame.height }?
                .frame
        }
        #endif
        return nil
    }
}
