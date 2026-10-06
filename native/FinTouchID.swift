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
    let terminalReview: TerminalConfigurationReview?
}

final class ConfigurationDetails: NSObject {
    let text: NSTextView
    let summary: String
    let details: String
    var expanded = false

    init(text: NSTextView, summary: String, details: String) {
        self.text = text
        self.summary = summary
        self.details = details
    }

    @objc func toggle(_ sender: NSButton) {
        expanded.toggle()
        text.string = expanded ? details : summary
        text.font = expanded ? NSFont.monospacedSystemFont(ofSize: 12, weight: .regular) : NSFont.systemFont(ofSize: 13)
        text.scrollRangeToVisible(NSRange(location: 0, length: 0))
        sender.title = expanded ? "Hide details" : "Details"
    }
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
context.localizedCancelTitle = "Cancel"
var availabilityError: NSError?
let available = context.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: &availabilityError) && context.biometryType == .touchID

if challenge.mode == "probe" {
    // Register the same accessory application identity used for authentication;
    // this creates no window and never evaluates an authentication policy.
    NSApplication.shared.setActivationPolicy(.accessory)
    emit(["version": 1, "status": available ? "available" : "unavailable", "mechanism": "touch-id", "biometryType": context.biometryType.rawValue, "errorCode": availabilityError?.code ?? 0, "approvalRequested": false,
          "applicationName": Bundle.main.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String ?? ProcessInfo.processInfo.processName,
          "runningApplicationName": NSRunningApplication.current.localizedName ?? "",
          "bundleIdentifier": Bundle.main.bundleIdentifier ?? ""])
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
let configuration = (binding as? [String: Any]).flatMap(configurationPresentation)
func brokerChannel(_ descriptor: Int32) -> Bool {
    var metadata = stat()
    guard fstat(descriptor, &metadata) == 0 else { return false }
    if descriptor == STDIN_FILENO {
        return protectedReviewInput(mode: UInt32(metadata.st_mode), owner: metadata.st_uid, links: UInt64(metadata.st_nlink), flags: fcntl(descriptor, F_GETFL))
    }
    return protectedReviewOutput(mode: UInt32(metadata.st_mode), links: UInt64(metadata.st_nlink))
}
let terminalReviewed = terminalConfigurationReviewed(challenge.terminalReview, binding: binding as? [String: Any] ?? [:], hash: bindingHash, nonce: nonce, operation: operation, expiry: expiryText, protectedChannels: brokerChannel(STDIN_FILENO) && brokerChannel(STDOUT_FILENO))
// A rejected terminal proof is an error, never a second custom review dialog.
if challenge.terminalReview != nil && !terminalReviewed { finish("invalid") }

// Present exact resolved execution details in a native modal before requesting Touch ID.
// The text view scrolls rather than truncating amounts, addresses, or bounds.
let application = NSApplication.shared
application.setActivationPolicy(.accessory)
let deadlineTimer = Timer(timeInterval: max(0, expiry.timeIntervalSinceNow), repeats: false) { _ in finish("expired") }
RunLoop.main.add(deadlineTimer, forMode: .common)
RunLoop.main.add(deadlineTimer, forMode: .modalPanel)
if !terminalReviewed {
    let alert = NSAlert()
    alert.alertStyle = .warning
    alert.messageText = configuration?.title ?? "Approve one Fin write"
    alert.informativeText = configuration == nil
        ? "\(displaySafe(operation))\n\(displaySafe(summary))\nInspect the exact accounts, network, assets, amounts, destination, and limits below. Touch ID approves only this operation."
        : "Review this connection. Touch ID approves only this configuration change."
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
    let exactDetails = "Exact execution binding:\n\(displaySafe(pretty))\n\nRequest hash: \(bindingHash)\nExpires: \(expiryText)"
    text.string = exactDetails
    scroll.documentView = text
    var detailsController: ConfigurationDetails?
    if let configuration = configuration {
        let summary = displaySafe(configuration.body)
        text.string = summary
        text.font = NSFont.systemFont(ofSize: 13)
        let accessory = NSView(frame: NSRect(x: 0, y: 0, width: 620, height: 390))
        scroll.frame.origin.y = 30
        accessory.addSubview(scroll)
        let controller = ConfigurationDetails(text: text, summary: summary, details: exactDetails)
        let button = NSButton(title: "Details", target: controller, action: #selector(ConfigurationDetails.toggle(_:)))
        button.frame = NSRect(x: 0, y: 0, width: 120, height: 26)
        accessory.addSubview(button)
        detailsController = controller
        alert.accessoryView = accessory
    } else {
        alert.accessoryView = scroll
    }
    application.activate(ignoringOtherApps: true)
    guard withExtendedLifetime(detailsController, { alert.runModal() }) == .alertFirstButtonReturn else { finish("cancelled") }
}
guard Date() < expiry else { finish("expired") }

// A fresh context and biometric-only policy prevent password fallback and previous unlock reuse.
let approvalReason = configurationAuthenticationReason(binding as? [String: Any] ?? [:])
    ?? "authorize the financial action reviewed in Fin"
context.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: approvalReason) { success, error in
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
