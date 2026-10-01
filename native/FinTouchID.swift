import AppKit
import CryptoKit
import Darwin
import Foundation
import LocalAuthentication

// A one-shot helper. The protected broker alone receives this process's inherited stdout.
// Probe mode never requests biometric approval and cannot produce an accepted response.
struct Challenge: Decodable {
    let version: Int
    let mode: String
    let nonce: String?
    let bindingHash: String?
    let canonicalBinding: String?
    let operation: String?
    let summary: String?
    let expiresAt: String?
}

func timestamp(_ date: Date) -> String {
    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return formatter.string(from: date)
}

func parseDate(_ text: String) -> Date? {
    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return formatter.date(from: text)
}

func displaySafe(_ text: String) -> String {
    // A token name or account label must not visually reorder exact addresses/amounts.
    return text.unicodeScalars.map { scalar -> String in
        let value = scalar.value
        if (value < 32 && value != 10 && value != 9) || value == 127 || value == 0x200E || value == 0x200F || (0x202A...0x202E).contains(value) || (0x2066...0x2069).contains(value) {
            return String(format: "\\u%04x", value)
        }
        return String(scalar)
    }.joined()
}

func emit(_ response: [String: Any]) -> Never {
    let data = (try? JSONSerialization.data(withJSONObject: response, options: [.sortedKeys])) ?? Data("{\"version\":1,\"status\":\"unavailable\"}".utf8)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
    exit(0)
}

let arguments = Array(CommandLine.arguments.dropFirst())
let input: Data
if arguments == ["--probe"] {
    input = Data("{\"version\":1,\"mode\":\"probe\"}".utf8)
} else if arguments.isEmpty {
    input = FileHandle.standardInput.readDataToEndOfFile()
} else {
    emit(["version": 1, "status": "invalid"])
}
guard input.count <= 65_536, let challenge = try? JSONDecoder().decode(Challenge.self, from: input), challenge.version == 1 else {
    emit(["version": 1, "status": "invalid"])
}

let context = LAContext()
context.touchIDAuthenticationAllowableReuseDuration = 0
context.localizedFallbackTitle = ""
context.localizedCancelTitle = "Cancel write"
var availabilityError: NSError?
let available = context.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: &availabilityError) && context.biometryType == .touchID

if challenge.mode == "probe" {
    emit(["version": 1, "status": available ? "available" : "unavailable", "mechanism": "touch-id", "biometryType": context.biometryType.rawValue, "errorCode": availabilityError?.code ?? 0, "approvalRequested": false])
}

guard challenge.mode == "approve", let nonce = challenge.nonce, nonce.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil,
      let bindingHash = challenge.bindingHash, let canonicalBinding = challenge.canonicalBinding,
      let operation = challenge.operation, let summary = challenge.summary,
      let expiryText = challenge.expiresAt, let expiry = parseDate(expiryText) else {
    emit(["version": 1, "status": "invalid"])
}

let base: [String: Any] = ["version": 1, "nonce": nonce, "bindingHash": bindingHash]
func finish(_ status: String, _ extra: [String: Any] = [:]) -> Never {
    var response = base
    response["status"] = status
    for (key, value) in extra { response[key] = value }
    context.invalidate()
    emit(response)
}

let actualHash = SHA256.hash(data: Data(canonicalBinding.utf8)).map { String(format: "%02x", $0) }.joined()
guard actualHash == bindingHash,
      let bindingData = canonicalBinding.data(using: .utf8),
      let binding = try? JSONSerialization.jsonObject(with: bindingData),
      binding is [String: Any], operation.count <= 256, summary.count <= 4_096,
      expiry > Date(), expiry.timeIntervalSinceNow <= 91 else { finish("expired") }
guard available else { finish("unavailable") }

// Present exact resolved execution details in a native modal before requesting Touch ID.
// The text view scrolls rather than truncating amounts, addresses, or bounds.
let application = NSApplication.shared
application.setActivationPolicy(.accessory)
let alert = NSAlert()
alert.alertStyle = .warning
alert.messageText = "Approve one Fin write"
alert.informativeText = "\(displaySafe(operation))\n\(displaySafe(summary))\nInspect the exact accounts, network, assets, amounts, destination, and limits below. Touch ID approves only this operation."
alert.addButton(withTitle: "Continue with Touch ID")
alert.addButton(withTitle: "Cancel")
let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 620, height: 360))
scroll.hasVerticalScroller = true
scroll.hasHorizontalScroller = false
scroll.borderType = .bezelBorder
let text = NSTextView(frame: scroll.bounds)
text.isEditable = false
text.isSelectable = true
text.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
text.textContainerInset = NSSize(width: 10, height: 10)
text.isVerticallyResizable = true
text.isHorizontallyResizable = false
text.autoresizingMask = [.width]
text.textContainer?.widthTracksTextView = true
let pretty = (try? JSONSerialization.data(withJSONObject: binding, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])).flatMap { String(data: $0, encoding: .utf8) } ?? canonicalBinding
text.string = "Exact execution binding:\n\(displaySafe(pretty))\n\nRequest hash: \(bindingHash)\nExpires: \(expiryText)"
scroll.documentView = text
alert.accessoryView = scroll
application.activate(ignoringOtherApps: true)
let deadlineTimer = Timer(timeInterval: max(0, expiry.timeIntervalSinceNow), repeats: false) { _ in finish("expired") }
RunLoop.main.add(deadlineTimer, forMode: .common)
RunLoop.main.add(deadlineTimer, forMode: .modalPanel)
guard alert.runModal() == .alertFirstButtonReturn else { finish("cancelled") }
guard Date() < expiry else { finish("expired") }

// A fresh context and biometric-only policy prevent password fallback and previous unlock reuse.
context.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: "Approve Fin \(displaySafe(operation)) once. Exact financial details were displayed in Fin.") { success, error in
    DispatchQueue.main.async {
        guard Date() < expiry else { finish("expired") }
        if success {
            finish("accepted", ["approvedAt": timestamp(Date()), "expiresAt": expiryText])
        }
        let code = (error as? LAError)?.code
        if code == .userCancel || code == .appCancel || code == .systemCancel { finish("cancelled") }
        if code == .authenticationFailed { finish("rejected") }
        finish("unavailable")
    }
}
// The broker also enforces the deadline and kills this one-shot child on timeout.
DispatchQueue.main.asyncAfter(deadline: .now() + max(0, expiry.timeIntervalSinceNow)) { finish("expired") }
application.run()
finish("unavailable")
