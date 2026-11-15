/// USB HID keyboard usages (page 7): the codes UIKeyboardHIDUsage reports
/// for hardware keys, used for the on-screen keys too.
enum HID {
    static let a: UInt16 = 0x04
    static let one: UInt16 = 0x1E
    static let zero: UInt16 = 0x27
    static let enter: UInt16 = 0x28
    static let escape: UInt16 = 0x29
    static let backspace: UInt16 = 0x2A
    static let tab: UInt16 = 0x2B
    static let space: UInt16 = 0x2C
    static let minus: UInt16 = 0x2D
    static let equal: UInt16 = 0x2E
    static let leftBracket: UInt16 = 0x2F
    static let rightBracket: UInt16 = 0x30
    static let backslash: UInt16 = 0x31
    static let semicolon: UInt16 = 0x33
    static let quote: UInt16 = 0x34
    static let grave: UInt16 = 0x35
    static let comma: UInt16 = 0x36
    static let period: UInt16 = 0x37
    static let slash: UInt16 = 0x38
    static let capsLock: UInt16 = 0x39
    static let f1: UInt16 = 0x3A
    static let printScreen: UInt16 = 0x46
    static let scrollLock: UInt16 = 0x47
    static let pause: UInt16 = 0x48
    static let insert: UInt16 = 0x49
    static let home: UInt16 = 0x4A
    static let pageUp: UInt16 = 0x4B
    static let delete: UInt16 = 0x4C
    static let end: UInt16 = 0x4D
    static let pageDown: UInt16 = 0x4E
    static let right: UInt16 = 0x4F
    static let left: UInt16 = 0x50
    static let down: UInt16 = 0x51
    static let up: UInt16 = 0x52
    static let application: UInt16 = 0x65
    static let leftControl: UInt16 = 0xE0
    static let leftShift: UInt16 = 0xE1
    static let leftAlt: UInt16 = 0xE2
    static let leftGUI: UInt16 = 0xE3
    static let rightControl: UInt16 = 0xE4
    static let rightShift: UInt16 = 0xE5
    static let rightAlt: UInt16 = 0xE6
    static let rightGUI: UInt16 = 0xE7

    /// F1 to F12.
    static func f(_ n: Int) -> UInt16 { f1 + UInt16(n - 1) }

    static func letter(_ c: Character) -> UInt16 {
        a + UInt16((c.asciiValue ?? 0x61) - 0x61)
    }

    static func isModifier(_ usage: UInt16) -> Bool { (leftControl...rightGUI).contains(usage) }

    /// The key, and whether Shift is needed, for a character typed on the
    /// system keyboard (US layout). Other characters have no key.
    static func key(for character: Character) -> (usage: UInt16, shift: Bool)? {
        if let ascii = character.asciiValue {
            switch ascii {
            case 0x61...0x7A: return (a + UInt16(ascii - 0x61), false)
            case 0x41...0x5A: return (a + UInt16(ascii - 0x41), true)
            case 0x31...0x39: return (one + UInt16(ascii - 0x31), false)
            case 0x30: return (zero, false)
            case 0x0A, 0x0D: return (enter, false)
            case 0x09: return (tab, false)
            case 0x20: return (space, false)
            default: break
            }
        }
        return punctuation[character]
    }

    private static let punctuation: [Character: (usage: UInt16, shift: Bool)] = [
        "!": (one, true), "@": (one + 1, true), "#": (one + 2, true), "$": (one + 3, true),
        "%": (one + 4, true), "^": (one + 5, true), "&": (one + 6, true), "*": (one + 7, true),
        "(": (one + 8, true), ")": (zero, true),
        "-": (minus, false), "_": (minus, true), "=": (equal, false), "+": (equal, true),
        "[": (leftBracket, false), "{": (leftBracket, true),
        "]": (rightBracket, false), "}": (rightBracket, true),
        "\\": (backslash, false), "|": (backslash, true),
        ";": (semicolon, false), ":": (semicolon, true),
        "'": (quote, false), "\"": (quote, true),
        "`": (grave, false), "~": (grave, true),
        ",": (comma, false), "<": (comma, true),
        ".": (period, false), ">": (period, true),
        "/": (slash, false), "?": (slash, true),
    ]
}
