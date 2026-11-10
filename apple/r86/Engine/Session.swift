import AVFoundation
import CoreGraphics
import Foundation
import Observation

/// A running VM: the r86 handle, its latest frame and its sound output.
/// The Rust core thread runs the machine; this object only talks to it.
@MainActor
@Observable
final class Session {
    enum Command: UInt32 {
        case pause = 0, resume, reset, powerButton, ctrlAltDel, releaseInput
    }

    let vm: OpaquePointer
    /// Small copy of the screen for the Library, refreshed twice a second.
    private(set) var thumbnail: CGImage?
    @ObservationIgnored private(set) var image: CGImage?
    @ObservationIgnored private var serial: UInt64 = 0
    @ObservationIgnored private var pixels = Data()
    @ObservationIgnored private var audio: AudioOutput?
    /// Security-scoped files (CD images) open while the VM runs.
    @ObservationIgnored private var scoped: [URL] = []
    private let directory: String

    init(machine: Machine) throws {
        var scoped: [URL] = []
        if let iso = machine.iso, let url = Bookmarks.access(iso, in: machine.directory) {
            scoped.append(url)
        }
        var error: UnsafeMutablePointer<CChar>?
        guard let vm = r86_vm_start(machine.directory, R86.firmware, &error) else {
            scoped.forEach { $0.stopAccessingSecurityScopedResource() }
            throw R86Error(message: R86.take(error) ?? "Cannot start \(machine.name)")
        }
        self.vm = vm
        self.directory = machine.directory
        self.scoped = scoped
        self.audio = AudioOutput(vm: vm)
    }

    func send(_ command: Command) {
        r86_vm_command(vm, command.rawValue)
    }

    func status() -> R86Status {
        var status = R86Status()
        r86_vm_status(vm, &status)
        return status
    }

    func takeMessages() -> [String] {
        var messages: [String] = []
        while let message = R86.take(r86_vm_take_message(vm)) {
            messages.append(message)
        }
        return messages
    }

    /// Copy the newest frame if it changed; returns it only when new.
    func newImage() -> CGImage? {
        var frame = R86Frame()
        let capacity = pixels.count
        let copied = pixels.withUnsafeMutableBytes { buffer in
            r86_vm_frame(vm, serial, buffer.baseAddress?.assumingMemoryBound(to: UInt8.self), capacity, &frame)
        }
        let needed = Int(frame.width) * Int(frame.height) * 4
        if !copied {
            // Grow once for a larger mode, then take the frame next time.
            if frame.serial != serial && needed > capacity { pixels = Data(count: needed) }
            return nil
        }
        serial = frame.serial
        guard needed > 0, let provider = CGDataProvider(data: pixels.prefix(needed) as CFData),
              let image = CGImage(width: Int(frame.width), height: Int(frame.height),
                                  bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: Int(frame.width) * 4,
                                  space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
                                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                                  provider: provider, decode: nil, shouldInterpolate: false,
                                  intent: .defaultIntent) else { return nil }
        self.image = image
        return image
    }

    func refreshThumbnail() {
        _ = newImage()
        if thumbnail !== image { thumbnail = image }
    }

    func key(_ code: UInt16, down: Bool) {
        r86_vm_key(vm, code, down)
    }

    func mouse(dx: Int32, dy: Int32, buttons: UInt32) {
        r86_vm_mouse(vm, dx, dy, buttons)
    }

    /// Insert a CD image picked in a file panel, or eject with nil.
    func changeCD(_ url: URL?) {
        if let url {
            let accessing = url.startAccessingSecurityScopedResource()
            Bookmarks.remember(url, in: directory)
            if accessing { scoped.append(url) }
        }
        withOptionalCString(url?.path) { r86_vm_change_cd(vm, $0) }
    }

    func setVolume(_ percent: Int) {
        r86_vm_set_volume(vm, UInt32(percent))
    }

    /// Turn off now: no guest shutdown. Disks are flushed.
    func close() {
        audio?.stop()
        audio = nil
        r86_vm_free(vm)
        scoped.forEach { $0.stopAccessingSecurityScopedResource() }
        scoped = []
    }
}

/// Plays the VM's 48 kHz stereo stream. The render callback only pulls
/// samples from r86; an empty queue plays silence.
final class AudioOutput {
    private static let maxFrames = 4096
    private let engine = AVAudioEngine()
    private let scratch = UnsafeMutablePointer<Int16>.allocate(capacity: maxFrames * 2)

    init?(vm: OpaquePointer) {
        // A failed init still runs deinit, which frees `scratch`.
        guard let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2) else {
            return nil
        }
        let scratch = scratch
        let source = AVAudioSourceNode(format: format) { _, _, frameCount, audioBufferList -> OSStatus in
            let wanted = Int(frameCount)
            let got = r86_vm_audio_read(vm, scratch, min(wanted, AudioOutput.maxFrames))
            let buffers = UnsafeMutableAudioBufferListPointer(audioBufferList)
            guard buffers.count >= 2,
                  let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
                  let right = buffers[1].mData?.assumingMemoryBound(to: Float.self) else { return noErr }
            for i in 0..<wanted {
                if i < got {
                    left[i] = Float(scratch[2 * i]) / 32768
                    right[i] = Float(scratch[2 * i + 1]) / 32768
                } else {
                    left[i] = 0
                    right[i] = 0
                }
            }
            return noErr
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
        do {
            try engine.start()
        } catch {
            return nil
        }
    }

    func stop() {
        engine.stop()
    }

    deinit {
        engine.stop()
        scratch.deallocate()
    }
}
