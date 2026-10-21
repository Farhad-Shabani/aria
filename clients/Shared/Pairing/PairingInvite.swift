import CryptoKit
import Foundation

/// Carried as `aria-pair://v1?instance=…&code=…&fp=…`. The fingerprint, not
/// the certificate, keeps the QR sparse enough to scan at a glance. No port:
/// Bonjour supplies it, so a Mac that moves keeps its pairing.
struct PairingInvite: Equatable {
    let instanceName: String
    let code: String
    /// SHA-256 over the certificate's DER bytes.
    let fingerprint: Data

    static let scheme = "aria-pair"
    static let version = "v1"

    var url: URL? {
        var components = URLComponents()
        components.scheme = Self.scheme
        components.host = Self.version
        components.queryItems = [
            URLQueryItem(name: "instance", value: instanceName),
            URLQueryItem(name: "code", value: code),
            URLQueryItem(name: "fp", value: Self.base64URL(fingerprint)),
        ]
        return components.url
    }

    init(instanceName: String, code: String, fingerprint: Data) {
        self.instanceName = instanceName
        self.code = code
        self.fingerprint = fingerprint
    }

    init?(url: URL) {
        guard url.scheme == Self.scheme, url.host == Self.version,
            let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems
        else { return nil }
        let values = Dictionary(items.map { ($0.name, $0.value ?? "") }, uniquingKeysWith: { first, _ in first })
        guard let instanceName = values["instance"], !instanceName.isEmpty,
            let code = values["code"], !code.isEmpty,
            let encoded = values["fp"], let fingerprint = Self.decodeBase64URL(encoded),
            fingerprint.count == SHA256.byteCount
        else { return nil }
        self.init(instanceName: instanceName, code: code, fingerprint: fingerprint)
    }

    static func fingerprint(ofDER der: Data) -> Data {
        Data(SHA256.hash(data: der))
    }

    /// URL-safe base64 without padding: it survives a QR code and a query
    /// string without escaping.
    private static func base64URL(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    private static func decodeBase64URL(_ text: String) -> Data? {
        var standard =
            text
            .replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while standard.count % 4 != 0 { standard.append("=") }
        return Data(base64Encoded: standard)
    }
}
