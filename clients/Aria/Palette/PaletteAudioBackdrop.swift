import SwiftUI

extension EnvironmentValues {
    /// The level of the audio Aria hears while the palette is open, or nil
    /// while it hears none.
    @Entry var audioLevel: LiveLevel?
}

/// What a footer shows behind its text while Aria hears audio. The only code
/// that names the effect: to change it, show another view of the level here.
struct PaletteAudioBackdrop: View {
    @Environment(\.audioLevel) private var level

    var body: some View {
        ZStack {
            if let level {
                PaletteSandBand(level: level)
                    .transition(.opacity)
            }
        }
        .animation(.easeInOut(duration: 0.4), value: level != nil)
    }
}
