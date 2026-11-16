// Run with script/apple_session_check.sh. Compile the production Swift session
// and screen against a recording C API; no guest or disk is opened.
import AppKit
import Foundation

private enum VMProbe {
    static var next = 1
    static var live = Set<OpaquePointer>()
    static var calls = 0
    static var frees = 0

    static func check(_ vm: OpaquePointer?) {
        precondition(Thread.isMainThread)
        precondition(vm != nil && live.contains(vm!), "UI callback used a closed VM")
        calls += 1
    }
}

@_cdecl("r86_vm_start")
func probeStart(_ directory: UnsafePointer<CChar>?, _ firmware: UnsafePointer<CChar>?,
                _ logs: UnsafePointer<CChar>?,
                _ width: UInt16, _ height: UInt16,
                _ error: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) -> OpaquePointer? {
    // Tokens are never dereferenced by the recording API.
    let vm = OpaquePointer(bitPattern: VMProbe.next)!
    VMProbe.next += 1
    VMProbe.live.insert(vm)
    return vm
}

@_cdecl("r86_vm_free")
func probeFree(_ vm: OpaquePointer?) {
    VMProbe.check(vm)
    VMProbe.live.remove(vm!)
    VMProbe.frees += 1
}

@_cdecl("r86_vm_command")
func probeCommand(_ vm: OpaquePointer?, _ command: UInt32) { VMProbe.check(vm) }

@_cdecl("r86_vm_key")
func probeKey(_ vm: OpaquePointer?, _ code: UInt16, _ down: Bool) { VMProbe.check(vm) }

@_cdecl("r86_vm_mouse")
func probeMouse(_ vm: OpaquePointer?, _ dx: Int32, _ dy: Int32, _ buttons: UInt32) {
    VMProbe.check(vm)
}

@_cdecl("r86_vm_hid_key")
func probeHID(_ vm: OpaquePointer?, _ usage: UInt16, _ down: Bool) { VMProbe.check(vm) }

@_cdecl("r86_vm_pointer")
func probePointer(_ vm: OpaquePointer?, _ x: UInt16, _ y: UInt16, _ buttons: UInt32, _ wheel: Int16) {
    VMProbe.check(vm)
}

@_cdecl("r86_vm_pointer_active")
func probePointerActive(_ vm: OpaquePointer?) -> Bool {
    VMProbe.check(vm)
    return true
}

@_cdecl("r86_vm_change_cd")
func probeCD(_ vm: OpaquePointer?, _ path: UnsafePointer<CChar>?) { VMProbe.check(vm) }

@_cdecl("r86_vm_set_volume")
func probeVolume(_ vm: OpaquePointer?, _ percent: UInt32) { VMProbe.check(vm) }

@_cdecl("r86_vm_set_remote")
func probeRemote(_ vm: OpaquePointer?, _ on: Bool) { VMProbe.check(vm) }

@_cdecl("r86_vm_remote_url")
func probeRemoteURL(_ vm: OpaquePointer?) -> UnsafeMutablePointer<CChar>? {
    VMProbe.check(vm)
    return nil
}

@_cdecl("r86_vm_log")
func probeLog(_ vm: OpaquePointer?) -> UnsafeMutablePointer<CChar>? {
    VMProbe.check(vm)
    return nil
}

@_cdecl("r86_vm_status")
func probeStatus(_ vm: OpaquePointer?, _ status: UnsafeMutablePointer<R86Status>?) {
    VMProbe.check(vm)
    status!.pointee = R86Status(state: 0, instructions_per_second: 1)
}

@_cdecl("r86_vm_take_message")
func probeMessage(_ vm: OpaquePointer?) -> UnsafeMutablePointer<CChar>? {
    VMProbe.check(vm)
    return nil
}

@_cdecl("r86_vm_frame")
func probeFrame(_ vm: OpaquePointer?, _ serial: UInt64, _ pixels: UnsafeMutablePointer<UInt8>?,
                _ capacity: Int, _ frame: UnsafeMutablePointer<R86Frame>?) -> Bool {
    VMProbe.check(vm)
    frame!.pointee = R86Frame(width: 1, height: 1, serial: 1)
    guard serial != 1, capacity >= 4, let pixels else { return false }
    for i in 0..<4 { pixels[i] = 255 }
    return true
}

@_cdecl("r86_vm_audio_read")
func probeAudio(_ vm: OpaquePointer?, _ samples: UnsafeMutablePointer<Int16>?, _ frames: Int) -> Int {
    // The audio thread stays silent. This check covers UI callbacks on main.
    return 0
}

// Library management is outside this check; fail if it is invoked accidentally.
@_cdecl("r86_library_list")
func probeList(_ root: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_presets")
func probePresets() -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_memory_choices")
func probeMemoryChoices() -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_detect_iso")
func probeDetect(_ path: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_detect_disk")
func probeDetectDisk(_ path: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_library_suggest_name")
func probeName(_ root: UnsafePointer<CChar>?, _ base: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? {
    fatalError()
}

@_cdecl("r86_library_create")
func probeCreate(_ root: UnsafePointer<CChar>?, _ name: UnsafePointer<CChar>?, _ os: UnsafePointer<CChar>?,
                 _ memory: UInt32, _ disk: UInt64, _ iso: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? {
    fatalError()
}

@_cdecl("r86_library_create_dos")
func probeCreateDOS(_ root: UnsafePointer<CChar>?, _ name: UnsafePointer<CChar>?, _ os: UnsafePointer<CChar>?,
                    _ memory: UInt32, _ disk: UInt64, _ iso: UnsafePointer<CChar>?,
                    _ share: UnsafePointer<CChar>?, _ setup: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? {
    fatalError()
}

@_cdecl("r86_library_create_existing")
func probeCreateExisting(_ root: UnsafePointer<CChar>?, _ name: UnsafePointer<CChar>?, _ os: UnsafePointer<CChar>?,
                         _ memory: UInt32, _ disk: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? {
    fatalError()
}

@_cdecl("r86_library_export")
func probeExport(_ directory: UnsafePointer<CChar>?, _ disk: UnsafePointer<CChar>?,
                 _ config: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) -> UnsafeMutablePointer<CChar>? {
    fatalError()
}

@_cdecl("r86_library_import")
func probeImport(_ root: UnsafePointer<CChar>?, _ config: UnsafePointer<CChar>?, _ disk: UnsafePointer<CChar>?,
                 _ directory: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) -> UnsafeMutablePointer<CChar>? {
    fatalError()
}

@_cdecl("r86_library_set")
func probeSet(_ directory: UnsafePointer<CChar>?, _ key: UnsafePointer<CChar>?,
              _ value: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_library_delete")
func probeDelete(_ directory: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>? { fatalError() }

@_cdecl("r86_string_free")
func probeStringFree(_ string: UnsafeMutablePointer<CChar>?) { fatalError() }

@main
struct SessionLifetimeCheck {
    @MainActor
    static func main() throws {
        _ = NSApplication.shared
        let machine = Machine(directory: "/unused", name: "Lifetime check", os: "Other", engine: .auto,
                              memoryMiB: 16, disk: "/unused/disk.vhd", diskMiB: 1, iso: nil,
                              cdrom: true, bootOrder: "disk", volume: 0, network: false, notes: "")
        let session = try Session(machine: machine)
        let view = GuestNSView(frame: .zero)
        view.session = session

        precondition(session.status().state == 0)
        precondition(session.pointerActive)
        session.pointer(x: 0, y: 0, buttons: 0, wheel: 0)
        session.setRemote(false)
        precondition(session.remoteURL() == nil)
        precondition(session.log() == nil)
        _ = session.newImage() // First call allocates the frame buffer.
        precondition(session.newImage() != nil)
        let beforeDetach = VMProbe.calls
        GuestScreen.dismantleNSView(view, coordinator: ())
        precondition(VMProbe.calls > beforeDetach, "a live screen must release input")
        precondition(VMProbe.frees == 0, "closing a screen must leave its VM running")

        // Match shutdown: free the VM while SwiftUI still retains its screen.
        view.session = session
        session.close()
        precondition(VMProbe.frees == 1)
        let afterClose = VMProbe.calls
        GuestScreen.dismantleNSView(view, coordinator: ())
        precondition(view.session == nil)
        lateCallbacks(session)
        session.close()
        precondition(VMProbe.calls == afterClose && VMProbe.frees == 1)

        // An old screen's callbacks must not reach a newly started session.
        let restarted = try Session(machine: machine)
        view.session = restarted
        lateCallbacks(session)
        precondition(VMProbe.calls == afterClose)
        precondition(restarted.status().state == 0)
        restarted.close()
        GuestScreen.dismantleNSView(view, coordinator: ())
        precondition(VMProbe.frees == 2 && VMProbe.live.isEmpty)
        print("PASS: live detach, shutdown detach, late callbacks, repeated close, restart isolation")
    }

    @MainActor
    static func lateCallbacks(_ session: Session) {
        session.send(.releaseInput)
        session.key(0, down: false)
        session.mouse(dx: 0, dy: 0, buttons: 0)
        session.pointer(x: 0, y: 0, buttons: 0, wheel: 0)
        precondition(!session.pointerActive)
        session.changeCD(nil)
        session.setVolume(0)
        session.setRemote(false)
        precondition(session.remoteURL() == nil)
        precondition(session.log() == nil)
        precondition(session.status().state == 2)
        precondition(session.takeMessages().isEmpty)
        precondition(session.newImage() == nil)
        session.refreshThumbnail()
    }
}
