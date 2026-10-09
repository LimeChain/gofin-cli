import Foundation
import Darwin
import CoreFoundation

struct ConfigurationPresentation {
    let title: String
    let body: String
}

// An application presentation budget, not a promise about macOS dialog sizing.
// Never truncate exact amounts midway or change the complete approval binding.
let authenticationReasonMaximumCharacters = 240

func compactAuthenticationReason(prefix: String, summary: String, maximumCharacters: Int = authenticationReasonMaximumCharacters) -> String {
    let visible = summary.unicodeScalars.map { scalar -> String in
        let value = scalar.value
        if CharacterSet.controlCharacters.contains(scalar) || value == 0x200E || value == 0x200F
            || (0x202A...0x202E).contains(value) || (0x2066...0x2069).contains(value) {
            return String(format: "\\u%04x", value)
        }
        return String(scalar)
    }.joined()
    // Only shorten complete EVM addresses; leave amounts, memo/tags and other
    // identifiers intact. Exact addresses remain in terminal/agent review.
    let addresses = try! NSRegularExpression(pattern: "(?<![A-Za-z0-9])0x[0-9a-fA-F]{40}(?![A-Za-z0-9])")
    var compact = visible
    for match in addresses.matches(in: visible, range: NSRange(visible.startIndex..., in: visible)).reversed() {
        guard let range = Range(match.range, in: compact) else { continue }
        let address = String(compact[range])
        compact.replaceSubrange(range, with: "\(address.prefix(6))…\(address.suffix(4))")
    }
    for (long, short) in [
        ("Withdraw API quantity ", "Withdraw "),
        ("approved maximum fee ", "fee cap "),
        ("maximum gross debit ", "debit cap "),
        ("minimum net receipt ", "min receipt "),
        ("USD ceilings: ", "USD caps: "),
    ] {
        compact = compact.replacingOccurrences(of: long, with: short)
    }
    let reason = prefix + compact
    if reason.count <= maximumCharacters { return reason }

    // Stop at a complete clause and explicitly disclose omitted details. This
    // also bounds escaped control text and unusually long account/token names.
    let suffix = " See terminal review for full details."
    let available = maximumCharacters - prefix.count - suffix.count - 1
    var clauses: [String] = []
    for clause in compact.components(separatedBy: ";") {
        let candidate = (clauses + [clause]).joined(separator: ";")
        if candidate.count > available { break }
        clauses.append(clause)
    }
    let synopsis = clauses.isEmpty ? "the reviewed operation." : clauses.joined(separator: ";") + ";"
    return prefix + synopsis + suffix
}

// The broker supplies this summary over its private challenge channel. It is
// presentation only: the helper still verifies and returns the complete binding
// hash/nonce, and the broker alone decides whether that response authorizes a write.
func financialAuthenticationReason(_ binding: [String: Any], summary: String) -> String? {
    guard binding["version"] as? Int == 1,
          let operation = binding["operation"] as? String, !operation.isEmpty,
          binding["execution"] is [String: Any],
          binding["financial_intent"] is [String: Any],
          !summary.isEmpty, summary.count <= 4096 else { return nil }
    if operation == "binance.deposit", summary.hasPrefix("Deposit ") {
        // macOS supplies the app name and approval context. Avoid a redundant
        // prefix so the named route, amount and native gas cap fit together.
        // USD ceilings remain in the full review and verified approval binding.
        // Strip only the broker's complete, trailing USD-ceiling annotation.
        let concise = summary.replacingOccurrences(
            of: #" USD ceilings: \$[0-9]+(?:\.[0-9]+)? movement, \$[0-9]+(?:\.[0-9]+)? fees\.$"#,
            with: "", options: .regularExpression)
        // Exceptionally long aliases use the explicit full-review fallback.
        return compactAuthenticationReason(prefix: "deposit ", summary: String(concise.dropFirst("Deposit ".count)), maximumCharacters: 180)
    }
    if operation == "bridge.transfer" {
        return bridgeAuthenticationReason(summary: summary)
    }
    return compactAuthenticationReason(prefix: "approve this action: ", summary: summary)
}

// The broker owns the grant and evaluates each concrete attempt separately.
// This approval is explicitly for the displayed operation boundaries, not an
// administrative action or an unrestricted wallet unlock.
func boundaryAuthenticationReason(_ binding: [String: Any], summary: String) -> String? {
    guard binding["version"] as? Int == 1,
          binding["kind"] as? String == "operation_boundaries",
          let operation = binding["operation"] as? String, !operation.isEmpty,
          !operation.hasPrefix("configuration:"), !operation.hasPrefix("policy."),
          !operation.hasPrefix("connections."),
          let intent = binding["intent_id"] as? String, !intent.isEmpty,
          binding["fixed"] is [String: Any], binding["parameters"] is [String: Any],
          let debits = binding["maximum_debits"] as? [[String: Any]], !debits.isEmpty,
          binding["maximum_costs"] is [[String: Any]],
          let duration = binding["valid_for_seconds"] as? NSNumber,
          CFGetTypeID(duration) != CFBooleanGetTypeID(), duration.doubleValue > 0,
          !summary.isEmpty, summary.count <= 4096 else { return nil }
    if operation == "bridge.transfer" {
        let fixed = binding["fixed"] as? [String: Any]
        return bridgeAuthenticationReason(
            summary: summary,
            sourceAddress: fixed?["source_address"] as? String,
            refundAddress: fixed?["refund_to"] as? String)
    }
    return compactAuthenticationReason(prefix: "approve these boundaries: ", summary: summary)
}

// Bridge boundary reviews contain additional step and scope detail for the
// terminal. The system prompt keeps the action and its material route bounds.
private func bridgeAuthenticationReason(
    summary: String,
    sourceAddress: String? = nil,
    refundAddress: String? = nil
) -> String {
    let clauses = summary.components(separatedBy: "; ")
    let amountPattern = #"(?i)\bbridge ([^;]+)"#
    let minimumPattern = #"(?i)Receive ≥([^;]+)"#
    let totalFeesPattern = #"(?i)Total fees up to (.+?)(?=;| Finite grant|$)"#
    let feesPattern = #"(?i)(?:Max fees|Cumulative max fees) (.+?)(?=;| Finite grant|$)"#
    let amount = firstCapture(amountPattern, in: summary)
    let minimum = firstCapture(minimumPattern, in: summary)
    let fees = firstCapture(totalFeesPattern, in: summary) ?? firstCapture(feesPattern, in: summary)
    let route = clauses.first(where: { $0.contains("→") })
    let routeDetails = route.flatMap(bridgeNetworkRoute)

    guard let amount, let routeDetails else {
        let remainder = summary.hasPrefix("Bridge ")
            ? String(summary.dropFirst("Bridge ".count)) : summary
        return compactAuthenticationReason(prefix: "bridge ", summary: remainder, maximumCharacters: 180)
    }

    var reason = "bridge \(bridgeClauseValue(amount)) from \(routeDetails.text)"
    let sameRefundWallet = sourceAddress != nil && refundAddress != nil
        && sourceAddress!.caseInsensitiveCompare(refundAddress!) == .orderedSame
    if let recipient = firstCapture(#"(?i)recipient ([^;]+)"#, in: summary),
       bridgeClauseValue(recipient).caseInsensitiveCompare(routeDetails.sourceAlias) != .orderedSame {
        reason += "; recipient \(bridgeClauseValue(recipient))"
    }
    if let approval = firstCapture(#"(?i)approve up to (.+?) for Relay spender"#, in: summary) {
        reason += "; includes allowance up to \(bridgeClauseValue(approval))"
    }
    if !sameRefundWallet,
       let refund = firstCapture(#"(?i)Refund recipient ([^;\.]+)"#, in: summary),
       bridgeClauseValue(refund).caseInsensitiveCompare(routeDetails.sourceAlias) != .orderedSame {
        reason += "; refund to \(bridgeClauseValue(refund))"
    }
    if let refundPaths = firstCapture(#"(?i)Refund paths (.+?)(?=\. Finite grant|$)"#, in: summary) {
        if isSameWalletZeroRefundPaths(refundPaths, sourceAddress: sourceAddress, refundAddress: refundAddress) {
            reason += "; refunds may be zero"
        } else {
            reason += "; refund path \(bridgeClauseValue(refundPaths))"
        }
    }
    if let minimum { reason += ". You’ll get at least \(bridgeClauseValue(minimum))" }
    if let fees { reason += ". Total fees won’t exceed \(bridgeClauseValue(fees))" }
    if !reason.hasSuffix(".") { reason += "." }
    return compactAuthenticationReason(prefix: "", summary: reason, maximumCharacters: 220)
}

private func isSameWalletZeroRefundPaths(
    _ paths: String,
    sourceAddress: String?,
    refundAddress: String?
) -> Bool {
    guard let sourceAddress, let refundAddress,
          sourceAddress.caseInsensitiveCompare(refundAddress) == .orderedSame else { return false }
    let shortAddress = "\(refundAddress.prefix(6))…\(refundAddress.suffix(4))".lowercased()
    let pathPattern = #"(?i)^(?:Ethereum Mainnet|Arbitrum One|Robinhood Chain) (0x[0-9a-f]{4}…[0-9a-f]{4}) ≥0 [A-Za-z0-9]+$"#
    let entries = paths.split(separator: ",").map {
        String($0).trimmingCharacters(in: .whitespaces)
    }
    guard !entries.isEmpty else { return false }
    return entries.allSatisfy { entry in
        guard let address = firstCapture(pathPattern, in: entry) else { return false }
        return address.lowercased() == shortAddress
    }
}

private func bridgeClauseValue(_ value: String) -> String {
    value.trimmingCharacters(in: CharacterSet(charactersIn: " .\t\n\r"))
}

private func firstCapture(_ pattern: String, in value: String) -> String? {
    guard let regex = try? NSRegularExpression(pattern: pattern),
          let match = regex.firstMatch(in: value, range: NSRange(value.startIndex..., in: value)),
          match.numberOfRanges > 1,
          let range = Range(match.range(at: 1), in: value) else { return nil }
    return String(value[range])
}

private func bridgeNetworkRoute(_ clause: String) -> (text: String, sourceAlias: String)? {
    let route = clause.components(separatedBy: " via ").first ?? clause
    let pieces = route.components(separatedBy: " → ")
    guard pieces.count == 2 else { return nil }
    let networks = ["Ethereum Mainnet", "Arbitrum One", "Robinhood Chain"]
    guard let source = networks.first(where: { pieces[0].hasSuffix($0) }),
          let destination = networks.first(where: { pieces[1].hasPrefix($0) }) else { return nil }
    let alias = String(pieces[0].dropLast(source.count))
        .trimmingCharacters(in: .whitespaces)
    let shortSource = source == "Ethereum Mainnet" ? "Ethereum" : source
    let shortDestination = destination == "Arbitrum One" ? "Arbitrum" : destination
    return ("\(shortSource) to \(shortDestination)", alias)
}

// One biometric action authorizes the complete named revision. Preserve the
// full plan name in the 240-character system reason; optional money details
// stay in the terminal review and canonical binding.
func financialPlanAuthenticationReason(_ binding: [String: Any], summary: String) -> String? {
    guard binding["version"] as? Int == 1,
          binding["kind"] as? String == "financial_plan",
          let planID = binding["plan_id"] as? String, !planID.isEmpty, planID.count <= 256,
          let revisionID = binding["revision_id"] as? String, !revisionID.isEmpty, revisionID.count <= 256,
          let plan = binding["plan"] as? [String: Any],
          let name = plan["name"] as? String, !name.isEmpty, name.unicodeScalars.count <= 80,
          name == binding["name"] as? String,
          name.trimmingCharacters(in: .whitespacesAndNewlines) == name,
          name.unicodeScalars.allSatisfy({ scalar in
              let value = scalar.value
              return !CharacterSet.controlCharacters.contains(scalar)
                  && value != 0x2028 && value != 0x2029
                  && value != 0x061c && value != 0x200e && value != 0x200f
                  && !(0x202a...0x202e).contains(value)
                  && !(0x2066...0x2069).contains(value)
          }),
          let steps = plan["steps"] as? [[String: Any]], !steps.isEmpty,
          let permissions = binding["permissions"] as? [[String: Any]], permissions.count == steps.count,
          let duration = binding["valid_for_seconds"] as? NSNumber,
          CFGetTypeID(duration) != CFBooleanGetTypeID(), duration.doubleValue > 0,
          !summary.isEmpty, summary.count <= 4096 else { return nil }
    for (step, permission) in zip(steps, permissions) {
        guard let stepID = step["step_id"] as? String, !stepID.isEmpty,
              permission["step_id"] as? String == stepID,
              let operation = step["operation"] as? String, !operation.isEmpty,
              !operation.hasPrefix("configuration:"), !operation.hasPrefix("policy."),
              !operation.hasPrefix("connections.") else { return nil }
        if let generated = permission["generated_input_fields"] {
            guard let fields = generated as? [String],
                  fields.allSatisfy({ !$0.isEmpty && $0.hasPrefix("/") }) else { return nil }
        }
        guard
              ((permission["permission"] as? [String: Any]).map { body in
                  body["spec"] is [String: Any]
                      && body["possible_credits"] is [[String: Any]]
                      && body["fee_assets"] is [[String: Any]]
              } ?? false)
              || ((permission["exact"] as? [String: Any]).map { exact in
                  guard let scope = exact["scope"] as? [String: Any] else { return false }
                  return scope["mode"] as? String == "exact"
                      && scope["maximum_debits"] is [[String: Any]]
                      && scope["maximum_costs"] is [[String: Any]]
                      && scope["possible_credits"] is [[String: Any]]
                      && (exact["definition_hash"] as? String)?.isEmpty == false
                      && (exact["connections_hash"] as? String)?.isEmpty == false
              } ?? false) else { return nil }
    }
    let revised = binding["replaces_revision_id"] as? String
    let lead = revised == nil
        ? "approve the plan “\(name)”"
        : "approve the revised plan “\(name)”"
    // The validated name is bounded to 80 Unicode scalars, so the complete
    // lead fits. Include duration/step count only as a whole optional suffix.
    guard lead.unicodeScalars.count <= authenticationReasonMaximumCharacters else { return nil }
    let durationSeconds = duration.uint64Value
    let details = " (\(steps.count) steps, \(durationSeconds) seconds)"
    let detailed = lead + details
    return detailed.unicodeScalars.count <= authenticationReasonMaximumCharacters ? detailed : lead
}

// LocalAuthentication prefixes this reason with the app name. Use a specific
// lower-case action, without repeating the product name or adding punctuation.
// Full configuration/permissions remain in the protected terminal review.
func configurationAuthenticationReason(_ binding: [String: Any]) -> String? {
    guard let movement = binding["financial_movement"] as? NSNumber,
          CFGetTypeID(movement) == CFBooleanGetTypeID(), !movement.boolValue,
          let authority = binding["future_financial_authority"] as? NSNumber,
          CFGetTypeID(authority) == CFBooleanGetTypeID(), !authority.boolValue,
          let action = binding["action"] as? String else { return nil }
    if action == "initialize" { return "finish setup for your Mac account" }
    if action == "policy_set" { return "change your financial policy" }
    guard ["add", "update", "remove"].contains(action),
          let connection = binding["connection"] as? [String: Any],
          let name = connection["name"] as? String, !name.isEmpty, name.count <= 96,
          name.unicodeScalars.allSatisfy({ CharacterSet.alphanumerics.contains($0) || "_-".unicodeScalars.contains($0) }),
          let provider = connection["provider"] as? String else { return nil }
    let providerName: String
    switch provider {
    case "binance": providerName = "Binance"
    case "ethereum": providerName = "Ethereum"
    case "evm": providerName = "EVM"
    case "coingecko": providerName = "CoinGecko"
    default: return nil
    }
    let verb = action == "add" ? "save" : action == "update" ? "update" : "remove"
    return compactAuthenticationReason(prefix: "", summary: "\(verb) the \(providerName) connection \"\(name)\"")
}

// Return nil whenever the complete configuration cannot be summarized. Call only
// after the canonical binding's hash has been verified by the approval helper.
func configurationPresentation(_ binding: [String: Any]) -> ConfigurationPresentation? {
    func boolean(_ value: Any?) -> Bool? {
        guard let value = value as? NSNumber, CFGetTypeID(value) == CFBooleanGetTypeID() else { return nil }
        return value.boolValue
    }
    func string(_ value: Any?) -> String? {
        guard let value = value as? String, !value.isEmpty, value.count <= 2048,
              value.rangeOfCharacter(from: .controlCharacters) == nil else { return nil }
        return value
    }
    func keys(_ object: [String: Any], _ allowed: [String]) -> Bool {
        Set(object.keys).isSubset(of: Set(allowed))
    }
    func list(_ value: Any?) -> [String]? {
        guard let values = value as? [Any] else { return nil }
        let result = values.compactMap(string)
        return result.count == values.count ? result : nil
    }
    let footer = "No funds move now. Every later financial write requires fresh Touch ID approval."
    guard boolean(binding["financial_movement"]) == false,
          boolean(binding["future_financial_authority"]) == false,
          let action = string(binding["action"]) else { return nil }

    // Setup includes policy and protocol deployments: retain their full exact
    // display until a separately reviewed renderer covers that configuration.
    if action == "initialize" { return nil }

    guard ["add", "update", "remove"].contains(action),
          keys(binding, ["action", "connection", "checks", "warnings", "warning", "configuration_hash", "financial_movement", "future_financial_authority"]),
          string(binding["configuration_hash"]) != nil,
          let connection = binding["connection"] as? [String: Any],
          let name = string(connection["name"]), let provider = string(connection["provider"]),
          let resource = string(connection["resource"]) else { return nil }
    let verb = action == "add" ? "Save" : action == "update" ? "Update" : "Remove"
    var lines = ["\(verb) protected credentials for connection: \(name)"]
    var warnings: [String] = []
    if let value = binding["warnings"] { guard let values = list(value) else { return nil }; warnings += values }
    if let value = binding["warning"] { guard let value = string(value) else { return nil }; warnings.append(value) }
    if action == "remove" && warnings.isEmpty { return nil }
    var expectedChecks = Set<String>()
    var uid: String? = nil
    var childEmail: String? = nil
    var signing = false
    var network: String? = nil
    if provider == "binance" {
        guard keys(connection, ["provider", "name", "resource", "credential_ref", "role", "parent", "email", "wallet_type", "physical_identity", "physical_identity_verified"]),
              let identity = string(connection["physical_identity"]),
              boolean(connection["physical_identity_verified"]) == true,
              string(connection["credential_ref"]) != nil,
              let role = string(connection["role"]), ["main", "sub"].contains(role),
              connection["wallet_type"] as? String == "SPOT",
              resource == "binance:uid:\(identity):SPOT" else { return nil }
        uid = identity
        lines.append("Binance \(role == "main" ? "main account" : "subaccount"): UID \(identity), SPOT wallet.")
        if role == "sub" {
            guard let parent = string(connection["parent"]), let email = string(connection["email"]) else { return nil }
            childEmail = email
            lines.append("Main connection: \(parent)\nChild email: \(email)")
            if action != "remove" {
                expectedChecks.formUnion(["parent child email", "child UID/email association"])
            }
        } else if !(connection["parent"] is NSNull) || !(connection["email"] is NSNull) { return nil }
        expectedChecks.formUnion(["credential authentication", "physical account identity", "reported API restrictions"])
    } else if provider == "ethereum" {
        guard keys(connection, ["provider", "name", "resource", "credential_ref", "network", "address", "signing_enabled", "assets"]),
              let address = string(connection["address"]), let configuredNetwork = string(connection["network"]),
              let enabled = boolean(connection["signing_enabled"]),
              let assets = connection["assets"] as? [[String: Any]],
              enabled ? string(connection["credential_ref"]) != nil : connection["credential_ref"] is NSNull else { return nil }
        signing = enabled
        if !enabled { lines[0] = "\(verb) read-only connection: \(name)" }
        network = configuredNetwork
        lines.append("Ethereum wallet: \(address)\nNetwork: \(configuredNetwork)\nSigning: \(enabled ? "enabled" : "disabled (read-only)").")
        for asset in assets {
            guard keys(asset, ["network", "reference", "decimals"]), let assetNetwork = string(asset["network"]),
                  let reference = string(asset["reference"]), let decimals = asset["decimals"] as? Int else { return nil }
            lines.append("Tracked asset: \(reference) on \(assetNetwork), \(decimals) decimals.")
        }
        expectedChecks.formUnion(["RPC chain", "wallet identity", "native balance read"])
    } else { return nil }
    if provider != "binance" { lines.append("Account identity: \(resource)") }
    if action != "remove" {
        guard let checks = binding["checks"] as? [[String: Any]], binding["warnings"] != nil else { return nil }
        var observed = Set<String>()
        for check in checks {
            guard let checkName = string(check["name"]), expectedChecks.contains(checkName), observed.insert(checkName).inserted,
                  let status = string(check["status"]) else { return nil }
            switch checkName {
            case "credential authentication":
                guard keys(check, ["name", "status"]), status == "passed" else { return nil }

            case "physical account identity":
                guard keys(check, ["name", "status", "uid", "source"]), status == "passed", string(check["uid"]) == uid,
                      check["source"] as? String == "GET /api/v3/account" else { return nil }
            case "reported API restrictions":
                let fields = [("enableReading", "Reading"), ("enableWithdrawals", "Withdrawals"), ("enableInternalTransfer", "Internal transfers"), ("permitsUniversalTransfer", "Universal transfers"), ("ipRestrict", "IP restriction"), ("enableSpotAndMarginTrading", "Spot / margin trading")]
                guard keys(check, ["name", "status", "permissions"]), status == "observed", let permissions = check["permissions"] as? [String: Any], keys(permissions, fields.map { $0.0 }) else { return nil }
                var enabledFields: [String] = []
                var disabledFields: [String] = []
                var unknownFields: [String] = []
                for (key, label) in fields {
                    if let enabled = boolean(permissions[key]) {
                        if enabled { enabledFields.append(label) } else { disabledFields.append(label) }
                    } else if permissions[key] == nil || permissions[key] is NSNull { unknownFields.append(label) }
                    else { return nil }
                }
                var permissionsText = ["Binance reports (financial writes untested):"]
                if !enabledFields.isEmpty { permissionsText.append("Enabled: " + enabledFields.joined(separator: ", ")) }
                if !disabledFields.isEmpty { permissionsText.append("Disabled: " + disabledFields.joined(separator: ", ")) }
                if !unknownFields.isEmpty { permissionsText.append("Unknown: " + unknownFields.joined(separator: ", ")) }
                lines.append(permissionsText.joined(separator: "\n"))
            case "parent child email":
                guard keys(check, ["name", "status", "email"]), status == "passed", string(check["email"]) == childEmail else { return nil }
            case "child UID/email association":
                guard keys(check, ["name", "status", "uid", "email"]), status == "human_confirmation_required", string(check["uid"]) == uid, string(check["email"]) == childEmail else { return nil }
            case "RPC chain":
                guard keys(check, ["name", "status", "chain_id"]), status == "passed", let chain = check["chain_id"] as? Int, network == "ethereum:\(chain)" else { return nil }
                lines.append("RPC chain identity checked: \(chain).")
            case "wallet identity":
                guard keys(check, ["name", "status", "signer_derived"]), status == "passed", boolean(check["signer_derived"]) == signing else { return nil }
            case "native balance read":
                guard keys(check, ["name", "status", "atoms"]), status == "passed", let atoms = string(check["atoms"]) else { return nil }
                lines.append("Native balance read: \(atoms) atomic units.")
            default: return nil
            }
        }
        guard observed == expectedChecks else { return nil }
    } else if binding["checks"] != nil { return nil }
    if action == "update" { lines.append("The old credentials will be replaced. Any obsolete credential that cannot be deleted will be reported for cleanup.") }
    if action == "remove" { lines.append("Fin will remove the saved connection and try to delete its credentials. External accounts, funds and positions remain; any credential cleanup failure will be reported.") }
    let visibleWarnings = warnings.map { "Warning: \($0)" }
    return ConfigurationPresentation(title: "\(verb) Fin connection", body: (visibleWarnings + lines + [footer]).joined(separator: "\n\n"))
}

// This private protocol field is emitted only after the root broker consumes an
// opaque protected-terminal proof. Missing or inconsistent proof refuses a
// configuration approval; no custom dialog replaces this review or Touch ID.
struct TerminalConfigurationReview: Decodable {
    let purpose: String
    let bindingHash: String
    let nonce: String
    let operation: String
    let expiresAt: String
}

func terminalConfigurationReviewed(_ review: TerminalConfigurationReview?, binding: [String: Any], hash: String, nonce: String, operation: String, expiry: String, protectedChannels: Bool) -> Bool {
    guard protectedChannels, let review = review, review.purpose == "configuration",
          review.bindingHash == hash, review.nonce == nonce,
          review.operation == operation, review.expiresAt == expiry,
          operation.hasPrefix("configuration:"),
          let action = binding["action"] as? String, ["initialize", "add", "update", "remove", "policy_set"].contains(action),
          let movement = binding["financial_movement"] as? NSNumber,
          CFGetTypeID(movement) == CFBooleanGetTypeID(), !movement.boolValue,
          let authority = binding["future_financial_authority"] as? NSNumber,
          CFGetTypeID(authority) == CFBooleanGetTypeID(), !authority.boolValue else { return false }
    return true
}

func protectedReviewInput(mode: UInt32, owner: UInt32, links: UInt64, flags: Int32) -> Bool {
    // Darwin pipe stat reports the inspecting process's UID, not its creator.
    // Use an unlinked root-owned private regular file, inherited read-only.
    mode & 0o170000 == 0o100000 && mode & 0o777 == 0o600 && owner == 0 && links == 0 && flags >= 0 && flags & O_ACCMODE == O_RDONLY
}

func protectedReviewOutput(mode: UInt32, links: UInt64) -> Bool {
    mode & 0o170000 == 0o010000 && links == 0
}
