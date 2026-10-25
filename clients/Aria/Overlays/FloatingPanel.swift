import AppKit
import SwiftUI

/// A non-activating overlay that ignores clicks so dictation keeps its destination focus.
@MainActor
final class FloatingPanel {
    private let width: CGFloat
    private let bottomMargin: CGFloat
    private var panel: NSPanel?
    // One hosting view for the panel's life: SwiftUI then diffs each new
    // root against the last, so view state such as a running animation
    // survives a content change.
    private var host: NSHostingView<AnyView>?
    /// Counts shows, so a fade-out that finishes after the next show leaves it alone.
    private var shows = 0

    init(width: CGFloat, bottomMargin: CGFloat) {
        self.width = width
        self.bottomMargin = bottomMargin
    }

    func show(_ content: some View) {
        shows += 1
        let panel = self.panel ?? makePanel()
        self.panel = panel

        // The root fills the window and centres the content in it, so the
        // window's width is never proposed to the text.
        let root = AnyView(content.frame(maxWidth: .infinity, maxHeight: .infinity))
        let height = NSHostingView(rootView: AnyView(content)).fittingSize.height
        if let host {
            host.rootView = root
        } else {
            let host = NSHostingView(rootView: root)
            // The window sizes the host, never the other way round.
            host.sizingOptions = []
            self.host = host
            panel.contentView = host
        }
        position(panel, height: height)
        // A show during a fade-out is the next press: cancel the fade.
        panel.alphaValue = 1
        panel.orderFrontRegardless()
    }

    func hide() {
        guard let panel, panel.isVisible else { return }
        let hiding = shows
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0.3
            panel.animator().alphaValue = 0
        } completionHandler: {
            // AppKit calls this on the main thread.
            MainActor.assumeIsolated {
                guard self.shows == hiding else { return }
                panel.orderOut(nil)
                panel.alphaValue = 1
                // An animated mark keeps its timeline running in a hidden
                // window; an empty root stops it until the next show.
                self.host?.rootView = AnyView(EmptyView())
            }
        }
    }

    private func makePanel() -> NSPanel {
        let panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: width, height: 44),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.level = .screenSaver
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary]
        // Without this a borderless panel can still be made key by the system.
        panel.becomesKeyOnlyIfNeeded = true
        return panel
    }

    private func position(_ panel: NSPanel, height: CGFloat) {
        guard let screen = NSScreen.main else { return }
        // The window is as wide as its widest content and clear around it,
        // so the content centres and nothing is cut to the last stage's
        // width. Only the height follows the content.
        let size = NSSize(width: width, height: height)
        host?.frame = NSRect(origin: .zero, size: size)
        let frame = screen.visibleFrame
        panel.setFrame(
            NSRect(
                x: frame.midX - size.width / 2,
                y: frame.minY + bottomMargin,
                width: size.width,
                height: size.height
            ),
            display: true
        )
    }
}
