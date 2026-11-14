import SwiftUI

extension EnvironmentValues {
    /// The row a search result opened, until that row has shown its mark.
    @Entry var revealedRow: Binding<SettingsRow?> = .constant(nil)
    /// Lets a revealed row scroll itself into view. Only the row knows when
    /// it has been built.
    @Entry var settingsScroll: ScrollViewProxy?
    /// The row to focus, until it takes focus, its page closes, or the
    /// sidebar gets a new pick or search.
    @Entry var focusRequest: Binding<SettingsRow?> = .constant(nil)
}

extension View {
    func settingsRow(_ row: SettingsRow) -> some View {
        modifier(RevealableRow(row: row))
    }
}

private struct RevealableRow: ViewModifier {
    static let markDuration = Duration.seconds(1.5)
    /// Matches a grouped form's row padding, so the mark reaches the
    /// separators. Measured on macOS 27 only.
    static let markOutset = EdgeInsets(top: -10.5, leading: -8, bottom: -10.5, trailing: -8)
    /// Concentric with the grouped section's corners, 4 points in from its sides.
    static let markCornerRadius: CGFloat = 6
    static let markOpacity = 0.16
    static let markIn = Animation.easeOut(duration: 0.15)
    static let markOut = Animation.easeInOut(duration: 0.6)

    let row: SettingsRow
    @Environment(\.revealedRow) private var revealed
    @Environment(\.settingsScroll) private var scroll
    @Environment(\.focusRequest) private var focusRequest
    @FocusState private var isFocused: Bool
    @AccessibilityFocusState private var isVoiceOverFocused: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        let isRevealed = revealed.wrappedValue == row
        content
            .focused($isFocused)
            .accessibilityFocused($isVoiceOverFocused)
            .id(row)
            // Gray, like `selectionFill`, reads as a pressed or disabled row.
            // The accent reads as found.
            .background {
                RoundedRectangle(cornerRadius: Self.markCornerRadius)
                    .fill(.tint.opacity(Self.markOpacity))
                    .padding(Self.markOutset)
                    .opacity(isRevealed ? 1 : 0)
            }
            .animation(reduceMotion ? nil : isRevealed ? Self.markIn : Self.markOut, value: isRevealed)
            // The mark's clock starts when the row appears. A page waiting for
            // the background service builds its rows only once settings arrive.
            .task(id: isRevealed) {
                guard isRevealed else { return }
                withAnimation(reduceMotion ? nil : .default) { scroll?.scrollTo(row, anchor: .center) }
                do {
                    try await Task.sleep(for: Self.markDuration)
                } catch {
                    return
                }
                // A new pick can land after the sleep ends but before SwiftUI
                // cancels this task. The reveal then belongs to another row.
                if revealed.wrappedValue == row { revealed.wrappedValue = nil }
            }
            // macOS moves keyboard focus here only while Keyboard navigation
            // is on. VoiceOver moves only on Return, so arrowing through
            // results leaves VoiceOver in the sidebar.
            .onChange(of: focusRequest.wrappedValue == row, initial: true) { _, isRequested in
                guard isRequested else { return }
                isFocused = true
                isVoiceOverFocused = true
                focusRequest.wrappedValue = nil
            }
    }
}
