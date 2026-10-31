import AppKit
import Carbon.HIToolbox

/// A key held with at least one modifier, as the daemon stores it.
///
/// Modifiers arrive named rather than as a mask: the settings file is
/// shared with platforms that have no Carbon constants.
struct Chord: Equatable {
    let keyCode: UInt16
    let control: Bool
    let option: Bool
    let shift: Bool
    let command: Bool

    var carbonModifiers: UInt32 {
        var mask: UInt32 = 0
        if control { mask |= UInt32(controlKey) }
        if option { mask |= UInt32(optionKey) }
        if shift { mask |= UInt32(shiftKey) }
        if command { mask |= UInt32(cmdKey) }
        return mask
    }

    var wire: Aria_Daemon_V1_Chord {
        var message = Aria_Daemon_V1_Chord()
        message.keyCode = UInt32(keyCode)
        message.control = control
        message.option = option
        message.shift = shift
        message.command = command
        return message
    }

    enum Capture: Equatable {
        case cancelled
        /// Delete alone, which clears the chord as recorders do.
        case cleared
        case refused(String)
        case chord(Chord)
    }

    static func captured(keyCode: UInt16, modifiers: NSEvent.ModifierFlags) -> Capture {
        guard keyCode != UInt16(kVK_Escape) else { return .cancelled }
        let control = modifiers.contains(.control)
        let option = modifiers.contains(.option)
        let shift = modifiers.contains(.shift)
        let command = modifiers.contains(.command)
        let bare = !(control || option || shift || command)
        if bare && [UInt16(kVK_Delete), UInt16(kVK_ForwardDelete)].contains(keyCode) {
            return .cleared
        }
        if bare {
            return .refused("Hold Control, Option, Shift, or Command as well.")
        }
        // ⌘Q, ⌘W, ⌘S belong to the app in front, and a registered chord is
        // consulted before them, everywhere.
        if command && !(control || option || shift) {
            return .refused("Command alone belongs to the app in front. Add another modifier.")
        }
        return .chord(Chord(keyCode: keyCode, control: control, option: option, shift: shift, command: command))
    }
}

extension Chord {
    /// Nil for a key code wider than the daemon's `u16`, or no modifier: a
    /// bare key would be taken away from every app.
    init?(wire: Aria_Daemon_V1_Chord) {
        guard let keyCode = UInt16(exactly: wire.keyCode),
            wire.control || wire.option || wire.shift || wire.command
        else { return nil }
        self.init(
            keyCode: keyCode, control: wire.control, option: wire.option, shift: wire.shift, command: wire.command)
    }

    /// A chord this build cannot use reads as none.
    init?(stored wire: Aria_Daemon_V1_Chord, isSet: Bool) {
        guard isSet else { return nil }
        self.init(wire: wire)
    }
}

extension Aria_Daemon_V1_Prompt {
    var storedChord: Chord? {
        Chord(stored: chord, isSet: hasChord)
    }
}
