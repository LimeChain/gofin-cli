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

// No custom windows: terminal/agent previews present details, and the system's
// biometric prompt identifies this one exact write. Configuration retains its
// protected terminal proof; removing a window must not bypass that proof.
let verifiedBinding = binding as? [String: Any] ?? [:]
let approvalReason: String
if verifiedBinding["action"] as? String == "operation_resolution" {
    guard challenge.terminalReview == nil else { finish("invalid_resolution_review") }
    switch operationResolutionApprovalReason(verifiedBinding) {
    case .success(let reason): approvalReason = reason
    case .failure(let failure): finish(failure.rawValue)
    }
} else if verifiedBinding["action"] as? String == "reservation_override" {
    guard challenge.terminalReview == nil,
          let reason = reservationOverrideAuthenticationReason(verifiedBinding) else { finish("invalid") }
    approvalReason = reason
 } else if verifiedBinding["execution_binding"] != nil || verifiedBinding["kind"] as? String == "financial_approval_review" || isFinancialApprovalBinding(verifiedBinding) {
    guard challenge.terminalReview == nil else { finish("invalid_financial_review") }
    switch typedFinancialApprovalReason(verifiedBinding) {
    case .success(let reason): approvalReason = reason
    case .failure(let failure): finish(failure.rawValue)
    }
} else {
    guard terminalReviewed, let reason = configurationAuthenticationReason(verifiedBinding) else { finish("invalid") }
    approvalReason = reason
}
let application = NSApplication.shared
application.setActivationPolicy(.accessory)
guard Date() < expiry else { finish("expired") }

// A fresh context and biometric-only policy prevent password fallback and previous unlock reuse.
context.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: nativeApprovalReasonForOS(approvalReason)) { success, error in
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
