import AppKit

/// Draws the words streaming has not typed yet as faint text at the caret
/// of the field they will land in.
@MainActor
final class GhostController {
    static let enabledKey = "ghostTextEnabled"
    static let enabledByDefault = true
    private static let clearingStages: [Aria_Daemon_V1_DictationStage] = [
        .segmentDelivered, .delivered, .failed, .cancelled,
    ]

    private let events: DaemonEvents
    private let panel = GhostPanel()
    private var state = GhostState()
    private var expiry: Task<Void, Never>?

    init(events: DaemonEvents) {
        self.events = events
    }

    /// The caller starts the stream once every overlay has registered.
    func start() {
        UserDefaults.standard.register(defaults: [Self.enabledKey: Self.enabledByDefault])
        state.setEnabled(UserDefaults.standard.bool(forKey: Self.enabledKey))
        events.onGhost { [weak self] snapshot in
            guard let self else { return }
            state.apply(snapshot, now: ProcessInfo.processInfo.systemUptime)
            render()
        }
        // Stages are never dropped, so they clear the ghost even when its
        // empty update is lost.
        events.onStage { [weak self] event in
            guard let self else { return }
            if let event, !Self.clearingStages.contains(event.stage) { return }
            state.clear()
            render()
        }
        NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.state.setEnabled(UserDefaults.standard.bool(forKey: Self.enabledKey))
                self.render()
            }
        }
    }

    private func render() {
        expiry?.cancel()
        guard state.isVisible, let placement = state.placement else {
            panel.hide()
            return
        }
        panel.show(state.text, at: placement)
        expiry = Task { [weak self] in
            try? await Task.sleep(for: .seconds(GhostState.expiry))
            guard !Task.isCancelled, let self else { return }
            state.expire(now: ProcessInfo.processInfo.systemUptime)
            render()
        }
    }
}

/// A click-through window over the other app. It never takes focus, so
/// typing keeps going to the field below.
@MainActor
final class GhostPanel {
    /// How strongly the ghost shows against the app's own text.
    private static let ink = 0.4
    /// Font size as a fraction of caret height, for apps that report no font.
    private static let caretToFontSize = 0.83

    private var panel: NSPanel?
    private let text = GhostTextView()

    func show(_ words: String, at placement: Aria_Daemon_V1_Placement) {
        guard let primary = NSScreen.screens.first else { return }
        let caret = Self.rect(placement.caret)
        let font = Self.font(placement, caretHeight: caret.height)
        let layout = GhostLayout(
            caret: caret,
            field: placement.hasField ? Self.rect(placement.field) : nil,
            primaryScreenHeight: primary.frame.height,
            ascender: font.ascender,
            descender: font.descender
        )
        let paragraph = NSMutableParagraphStyle()
        paragraph.firstLineHeadIndent = layout.firstLineIndent
        paragraph.minimumLineHeight = layout.lineHeight
        paragraph.maximumLineHeight = layout.lineHeight
        paragraph.lineBreakMode = .byWordWrapping
        text.content = NSAttributedString(
            string: words,
            attributes: [
                .font: font,
                .foregroundColor: NSColor.labelColor.withAlphaComponent(Self.ink),
                .paragraphStyle: paragraph,
                .baselineOffset: layout.baselineOffset,
            ]
        )
        let panel = self.panel ?? makePanel()
        self.panel = panel
        panel.setFrame(layout.frame, display: true)
        text.frame = NSRect(origin: .zero, size: layout.frame.size)
        text.needsDisplay = true
        panel.orderFrontRegardless()
    }

    /// Instant, never faded: a lingering ghost reads as text that landed.
    func hide() {
        panel?.orderOut(nil)
    }

    private func makePanel() -> NSPanel {
        let panel = NSPanel(
            contentRect: .zero,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.level = .statusBar
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient, .ignoresCycle]
        panel.animationBehavior = .none
        panel.becomesKeyOnlyIfNeeded = true
        // Screen readers would announce words that are not in the field.
        panel.setAccessibilityElement(false)
        panel.contentView = text
        return panel
    }

    private static func font(_ placement: Aria_Daemon_V1_Placement, caretHeight: CGFloat) -> NSFont {
        if placement.fontSize > 0, let font = NSFont(name: placement.fontName, size: placement.fontSize) {
            return font
        }
        return .systemFont(ofSize: caretHeight * caretToFontSize)
    }

    private static func rect(_ rect: Aria_Daemon_V1_Rect) -> CGRect {
        CGRect(x: rect.x, y: rect.y, width: rect.width, height: rect.height)
    }
}

/// Text drawn from the top-left with no insets, so the first line meets the caret exactly.
private final class GhostTextView: NSView {
    var content = NSAttributedString()

    override var isFlipped: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        content.draw(with: bounds, options: [.usesLineFragmentOrigin])
    }
}
