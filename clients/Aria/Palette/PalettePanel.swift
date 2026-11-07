import AppKit
import SwiftUI

/// A floating panel that takes keys without activating Aria, so the app in
/// front stays in front and a prompt run from here still types into it.
@MainActor
final class PalettePanel {
    static let size = NSSize(width: 760, height: 480)
    private static let cornerRadius: CGFloat = 14
    /// How far above the screen's middle the panel sits, as a share of the space above it when
    /// centered, as Spotlight does.
    private static let lift: CGFloat = 0.35
    /// Black laid over the blur, so the palette reads darker than the
    /// material alone.
    private static let dimming: CGFloat = 0.35

    private let panel: KeyPanel
    private let host = NSHostingView(rootView: AnyView(EmptyView()))

    /// - Parameters:
    ///   - resignedKey: Called when clicking elsewhere takes the keys away,
    ///     but not when a sheet does.
    ///   - pressed: Every key, before the view in focus sees it, unless an
    ///     input method is composing. Returns whether it was taken.
    init(resignedKey: @escaping @MainActor () -> Void, pressed: @escaping @MainActor (NSEvent) -> Bool) {
        panel = KeyPanel(resignedKey: resignedKey, pressed: pressed)
        configure()
    }

    var isVisible: Bool { panel.isVisible }

    func show(_ content: some View) {
        host.rootView = AnyView(content)
        position()
        panel.makeKeyAndOrderFront(nil)
    }

    /// Shows `openPanel` as a sheet, so the palette stays open while the user chooses. `nil` when
    /// they cancel.
    func choose(_ openPanel: NSOpenPanel) async -> URL? {
        guard await openPanel.beginSheetModal(for: panel) == .OK else { return nil }
        return openPanel.url
    }

    func hide() {
        panel.orderOut(nil)
        host.rootView = AnyView(EmptyView())
    }

    private func configure() {
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.level = .floating
        panel.isMovableByWindowBackground = false
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]

        let blur = NSVisualEffectView(frame: NSRect(origin: .zero, size: Self.size))
        blur.material = .hudWindow
        blur.blendingMode = .behindWindow
        blur.state = .active
        blur.wantsLayer = true
        blur.layer?.cornerRadius = Self.cornerRadius
        blur.layer?.masksToBounds = true

        let dim = NSView(frame: blur.bounds)
        dim.wantsLayer = true
        dim.layer?.backgroundColor = NSColor.black.withAlphaComponent(Self.dimming).cgColor
        dim.autoresizingMask = [.width, .height]
        blur.addSubview(dim)

        host.sizingOptions = []
        host.frame = blur.bounds
        host.autoresizingMask = [.width, .height]
        blur.addSubview(host)
        panel.contentView = blur
    }

    private func position() {
        guard let screen = NSScreen.main else { return }
        let frame = screen.visibleFrame
        let spare = frame.height - Self.size.height
        let origin = NSPoint(
            x: frame.midX - Self.size.width / 2,
            y: frame.minY + spare / 2 + spare * Self.lift / 2
        )
        panel.setFrame(NSRect(origin: origin, size: Self.size), display: true)
    }
}

/// A borderless panel refuses keys unless told otherwise.
private final class KeyPanel: NSPanel {
    private let resignedKey: @MainActor () -> Void
    private let pressed: @MainActor (NSEvent) -> Bool

    init(resignedKey: @escaping @MainActor () -> Void, pressed: @escaping @MainActor (NSEvent) -> Bool) {
        self.resignedKey = resignedKey
        self.pressed = pressed
        super.init(
            contentRect: NSRect(origin: .zero, size: PalettePanel.size),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
    }

    /// Keys reach the palette before a text field's editor, which would
    /// otherwise take ⌃N, ⌃P and Esc. A key composing text in an input
    /// method stays with it.
    override func sendEvent(_ event: NSEvent) {
        if event.type == .keyDown, !isComposing, pressed(event) { return }
        super.sendEvent(event)
    }

    private var isComposing: Bool {
        (firstResponder as? NSTextView)?.hasMarkedText() ?? false
    }

    override var canBecomeKey: Bool { true }

    /// A sheet takes the keys from the panel without closing the palette.
    override func resignKey() {
        super.resignKey()
        guard attachedSheet == nil else { return }
        resignedKey()
    }
}
