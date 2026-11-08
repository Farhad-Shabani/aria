extension Duration {
    /// A finished length, such as “12m 4s” or “1h 2m”.
    var lengthLabel: String {
        formatted(.units(allowed: [.hours, .minutes, .seconds], width: .abbreviated, maximumUnitCount: 2))
    }
}
