extension AriaKey {
    func gesture(for combo: ShortcutCombo) -> String {
        "\(title) + \(combo.symbol)"
    }
}
