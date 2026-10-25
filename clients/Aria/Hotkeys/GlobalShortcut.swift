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
    private static var lastID: UInt32 = 0
    private static var handler: EventHandlerRef?
    /// Every registered shortcut, so recording a new chord can let them all
    /// go: a registered chord fires before the Settings window sees the key.
    private static var registered: [UInt32: GlobalShortcut] = [:]
    private static var paused = false

    private let id: UInt32
    private var keyCode: UInt32 = 0
    private var modifiers: UInt32 = 0
    private var hotKey: EventHotKeyRef?
    private var action: (@MainActor () -> Void)?

    init() {
        Self.lastID += 1
        id = Self.lastID
    }

    /// Replaces whatever was registered before, so a settings change lands
    /// without a restart. False when Carbon refuses the chord.
    @discardableResult
    func register(keyCode: UInt32, modifiers: UInt32, action: @escaping @MainActor () -> Void) -> Bool {
        unregister()
        self.keyCode = keyCode
        self.modifiers = modifiers
        self.action = action
        Self.registered[id] = self
        return Self.paused || take()
    }

    func unregister() {
        release()
        action = nil
        Self.registered[id] = nil
    }

    /// Runs the shortcut Carbon reports pressed. False for an id this app
    /// did not register.
    fileprivate static func press(_ id: UInt32) -> Bool {
        guard let action = registered[id]?.action else { return false }
        action()
        return true
    }

    static func pause(_ pausing: Bool) {
        guard pausing != paused else { return }
        paused = pausing
        for shortcut in registered.values {
            if pausing {
                shortcut.release()
            } else {
                shortcut.take()
            }
        }
    }

    @discardableResult
    private func take() -> Bool {
        Self.installHandler()
        var reference: EventHotKeyRef?
        let status = RegisterEventHotKey(
            keyCode,
            modifiers,
            EventHotKeyID(signature: ariaSignature, id: id),
            GetEventDispatcherTarget(),
            0,
            &reference
        )
        guard status == noErr, let reference else {
            log.error("Registration refused: OSStatus \(status, privacy: .public)")
            return false
        }
        hotKey = reference
        return true
    }

    private func release() {
        if let hotKey {
            UnregisterEventHotKey(hotKey)
        }
        hotKey = nil
    }

    /// One handler serves every shortcut and outlives each registration: it
    /// dispatches by hot key id.
    private static func installHandler() {
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

/// 'ARIA'. With each shortcut's id it separates these chords from ones
/// another framework in the process registered.
private let ariaSignature = OSType(0x4152_4941)

/// A bare C function pointer that captures nothing, so it finds the pressed
/// shortcut by hot key id.
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
    guard status == noErr, pressed.signature == ariaSignature else {
        return OSStatus(eventNotHandledErr)
    }
    let id = pressed.id
    // Carbon dispatches only on the main thread; this traps if that changes.
    return MainActor.assumeIsolated {
        GlobalShortcut.press(id) ? noErr : OSStatus(eventNotHandledErr)
    }
}
