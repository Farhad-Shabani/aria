import Carbon.HIToolbox
import os

private let log = Logger(subsystem: "io.github.farhad-shabani.aria", category: "GlobalShortcut")

/// One globally registered chord.
///
/// Carbon registration is the reason this exists rather than a second
/// `CGEventTap`: it needs no Accessibility grant, which an ad-hoc signature
/// voids on every rebuild. The cost is that Carbon only dispatches into an
/// `NSApplication` run loop, which is why the app registers the chord and
/// the daemon cannot.
///
/// Registration is nonexclusive, so another app may receive the same chord.
@MainActor
final class GlobalShortcut {
    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?

    /// Replaces whatever was registered before, so a settings change lands
    /// without a restart. False when Carbon refuses the chord.
    @discardableResult
    func register(keyCode: UInt32, modifiers: UInt32, action: @escaping () -> Void) -> Bool {
        unregister()
        installHandler()
        var reference: EventHotKeyRef?
        let status = RegisterEventHotKey(
            keyCode,
            modifiers,
            EventHotKeyID(signature: ariaSignature, id: askShortcutID),
            GetEventDispatcherTarget(),
            0,
            &reference
        )
        guard status == noErr, let reference else {
            log.error("Registration refused: OSStatus \(status, privacy: .public)")
            return false
        }
        hotKey = reference
        pressAction = action
        return true
    }

    func unregister() {
        if let hotKey {
            UnregisterEventHotKey(hotKey)
        }
        hotKey = nil
        pressAction = nil
    }

    /// The handler outlives each registration: it dispatches by hot key id,
    /// so re-registering reuses it.
    private func installHandler() {
        guard handler == nil else { return }
        var pressedKind = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        InstallEventHandler(
            GetEventDispatcherTarget(),
            handleHotKey,
            1,
            &pressedKind,
            nil,
            &handler
        )
    }
}

/// 'ARIA'. With the id below it separates this chord from one another
/// framework in the process registered.
private let ariaSignature = OSType(0x4152_4941)
private let askShortcutID: UInt32 = 1

/// The Carbon callback is a bare C function pointer and captures nothing, so
/// what a press runs lives here. Only `GlobalShortcut` writes it and only
/// this callback reads it, both on the main thread — Carbon dispatches
/// nowhere else.
nonisolated(unsafe) private var pressAction: (() -> Void)?

private func handleHotKey(
    _ call: EventHandlerCallRef?,
    _ event: EventRef?,
    _ context: UnsafeMutableRawPointer?
) -> OSStatus {
    var pressed = EventHotKeyID()
    let status = GetEventParameter(
        event,
        EventParamName(kEventParamDirectObject),
        EventParamType(typeEventHotKeyID),
        nil,
        MemoryLayout<EventHotKeyID>.size,
        nil,
        &pressed
    )
    guard status == noErr, pressed.signature == ariaSignature, pressed.id == askShortcutID else {
        return OSStatus(eventNotHandledErr)
    }
    pressAction?()
    return noErr
}
