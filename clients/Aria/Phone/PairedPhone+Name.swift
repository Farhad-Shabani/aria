import UniformTypeIdentifiers

extension PairedPhone {
    /// The model's name, such as "iPhone 17 Pro", or "iPhone" when macOS
    /// doesn't know the model.
    var name: String {
        hardwareModel.flatMap(Self.modelName) ?? "iPhone"
    }

    /// macOS's type registry names device models for Finder, and learns new
    /// ones with each macOS update. The tag class is undocumented.
    static func modelName(_ hardwareModel: String) -> String? {
        let deviceModel = UTTagClass(rawValue: "com.apple.device-model-code")
        guard let type = UTType(tag: hardwareModel, tagClass: deviceModel, conformingTo: nil), !type.isDynamic
        else { return nil }
        return type.localizedDescription
    }
}
