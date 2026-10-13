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

// LocalAuthentication appends its own final period to the app/reason sentence.
// Strip only terminal full stops and whitespace at that call boundary; keep
// the complete validated reason and every internal character unchanged.
func nativeApprovalReasonForOS(_ reason: String) -> String {
    var end = reason.endIndex
    while end > reason.startIndex {
        let previous = reason.index(before: end)
        let character = reason[previous]
        guard character == "." || character.isWhitespace else { break }
        end = previous
    }
    return String(reason[..<end])
}

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
    if ammOperationPresentation(operation) != nil {
        return ammStandaloneAuthenticationReason(binding, summary: summary)
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
    if ammOperationPresentation(operation) != nil {
        return ammBoundarySynopsis(binding, summary: summary)
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

private func presentationString(_ value: Any?, maximum: Int = 256) -> String? {
    guard let text = value as? String, !text.isEmpty, text.count <= maximum,
          text.unicodeScalars.allSatisfy({ !CharacterSet.controlCharacters.contains($0) }) else { return nil }
    return text
}

private func presentationInteger(_ value: Any?, minimum: UInt64 = 0, maximum: UInt64 = UInt64.max) -> UInt64? {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID(),
          number.doubleValue.isFinite, number.doubleValue.rounded() == number.doubleValue,
          number.doubleValue >= Double(minimum), number.doubleValue <= Double(maximum) else { return nil }
    return number.uint64Value
}

private func presentationDecimal(_ value: Any?, positive: Bool = true) -> String? {
    guard let text = presentationString(value, maximum: 100),
          text.range(of: #"^(0|[1-9][0-9]*)(\.[0-9]+)?$"#, options: .regularExpression) != nil,
          !positive || text.range(of: #"[1-9]"#, options: .regularExpression) != nil else { return nil }
    return text
}

private func decimalAtMost(_ candidate: String, _ limit: String) -> Bool {
    let candidateParts = candidate.split(separator: ".", omittingEmptySubsequences: false)
    let limitParts = limit.split(separator: ".", omittingEmptySubsequences: false)
    guard !candidateParts.isEmpty, !limitParts.isEmpty else { return false }
    let leftInteger = String(candidateParts[0])
    let rightInteger = String(limitParts[0])
    if leftInteger.count != rightInteger.count { return leftInteger.count < rightInteger.count }
    if leftInteger != rightInteger { return leftInteger < rightInteger }
    let leftFraction = candidateParts.count > 1 ? String(candidateParts[1]) : ""
    let rightFraction = limitParts.count > 1 ? String(limitParts[1]) : ""
    let width = max(leftFraction.count, rightFraction.count)
    let left = leftFraction.padding(toLength: width, withPad: "0", startingAt: 0)
    let right = rightFraction.padding(toLength: width, withPad: "0", startingAt: 0)
    return left <= right
}

private func presentationAddress(_ value: Any?) -> String? {
    guard let text = presentationString(value, maximum: 42),
          text.range(of: #"^0x[0-9a-fA-F]{40}$"#, options: .regularExpression) != nil else { return nil }
    return text.lowercased()
}

private func presentationAssetEndpoint(_ value: String) -> Bool {
    value == "native" || presentationAddress(value) != nil
}

func approvalDuration(_ seconds: UInt64) -> String {
    let units: [(UInt64, String)] = [(3_600, "hour"), (60, "minute"), (1, "second")]
    var remaining = seconds
    var parts: [String] = []
    for (unitSeconds, name) in units {
        let value = remaining / unitSeconds
        remaining %= unitSeconds
        if value > 0 {
            parts.append("\(value) \(name)\(value == 1 ? "" : "s")")
        }
    }
    return parts.isEmpty ? "0 seconds" : parts.prefix(2).joined(separator: " ")
}

private func safeAMMLabel(_ raw: String?) -> String? {
    guard let raw else { return nil }
    let label = raw.trimmingCharacters(in: .whitespaces)
    guard !label.isEmpty, label.utf8.count <= 32,
          label.range(of: #"^[A-Za-z][A-Za-z0-9._-]*(?: [A-Za-z][A-Za-z0-9._-]*)*$"#, options: .regularExpression) != nil,
          !label.lowercased().hasPrefix("0x") else { return nil }
    return label
}

// Asset names are presentation-only broker prose. Numeric authorization bounds
// below always come from the structured binding, never this summary.
private func ammLabels(summary: String, bounded: Bool, inputReference: String? = nil,
                       outputReference: String? = nil) -> (input: String, output: String) {
    let pattern = bounded
        ? #"^Swap up to [^ ]+ (.+?) for (.+?);"#
        : #"^Swap [^ ]+ (.+?) for at least [^ ]+ (.+?);"#
    let proseInput = firstCapture(pattern, in: summary)
    // Capture the second label separately to avoid accepting arbitrary clauses.
    let outputPattern = bounded
        ? #"^Swap up to [^ ]+ .+? for (.+?);"#
        : #"^Swap [^ ]+ .+? for at least [^ ]+ (.+?);"#
    let proseOutput = firstCapture(outputPattern, in: summary)
    return (
        safeAMMLabel(proseInput) ?? (inputReference == "native" ? "ETH" : "token"),
        safeAMMLabel(proseOutput) ?? (outputReference == "native" ? "ETH" : "token")
    )
}

func reviewedNetworkName(_ network: String) -> String {
    guard network.hasPrefix("ethereum:"), let chainID = UInt64(network.dropFirst("ethereum:".count)) else {
        return "the reviewed network"
    }
    switch chainID {
    case 1: return "Ethereum"
    case 42161: return "Arbitrum"
    case 4663: return "Robinhood Chain"
    default: return "the reviewed network"
    }
}

// Keep operation identities closed: the broker's prose never selects a DEX.
private struct AMMOperationPresentation {
    let dex: String
    let brand: String
    let feeTiers: [UInt64]
    let additionalV3: Bool
}

private func ammOperationPresentation(_ operation: String) -> AMMOperationPresentation? {
    switch operation {
    case "uniswap.swap": return AMMOperationPresentation(dex: "uniswap", brand: "Uniswap", feeTiers: [100, 500, 3000, 10000], additionalV3: false)
    case "ramses.swap": return AMMOperationPresentation(dex: "ramses", brand: "Ramses", feeTiers: [], additionalV3: false)
    case "sushi.swap": return AMMOperationPresentation(dex: "sushi", brand: "Sushi", feeTiers: [100, 500, 3000, 10000], additionalV3: true)
    case "pancakeswap.swap": return AMMOperationPresentation(dex: "pancakeswap", brand: "PancakeSwap", feeTiers: [100, 500, 2500, 10000], additionalV3: true)
    default: return nil
    }
}

private func absentPresentationValue(_ value: Any?) -> Bool {
    value == nil || value is NSNull
}

// New fixed-fee venues always carry explicit deployment identities.
// Legacy Uniswap bindings intentionally retain their existing shape.
private func additionalV3Identity(_ scope: [String: Any], operation: AMMOperationPresentation, router: Any?) -> Bool {
    guard operation.additionalV3 else { return true }
    let zero = "0x" + String(repeating: "0", count: 40)
    guard scope["dex"] as? String == operation.dex,
          scope["protocol"] as? String == "v3",
          let factory = presentationAddress(scope["factory"]), factory != zero,
          let router = presentationAddress(router), router != zero,
          absentPresentationValue(scope["tick_spacing"]),
          absentPresentationValue(scope["max_pool_fee_ppm"]),
          absentPresentationValue(scope["swap_fee_ppm"]),
          absentPresentationValue(scope["v4_pool_key"]) else { return false }
    if operation.dex == "pancakeswap" {
        guard let deployer = presentationAddress(scope["pool_deployer"]), deployer != zero else { return false }
    } else if !absentPresentationValue(scope["pool_deployer"]) { return false }
    return true
}

// Concrete writes retain one exact pool. New consent scopes bind each allowed
// fee tier to a reviewed pool; old scopes keep their exact single-pool meaning.
private func additionalV3PoolScope(_ scope: [String: Any], operation: AMMOperationPresentation,
                                   bounds: [String: Any]? = nil) -> Bool {
    guard operation.additionalV3 else { return true }
    let zero = "0x" + String(repeating: "0", count: 40)
    guard let bounds else {
        return scope["approved_pools"] == nil
            && presentationAddress(scope["pool"]).map({ $0 != zero }) == true
    }
    guard let rawTiers = bounds["allowed_fee_tiers"] as? [Any],
          !rawTiers.isEmpty, rawTiers.count <= operation.feeTiers.count else { return false }
    let tiers = rawTiers.compactMap { presentationInteger($0, minimum: 1, maximum: 10000) }
    guard tiers.count == rawTiers.count, Set(tiers).count == tiers.count,
          tiers.allSatisfy({ operation.feeTiers.contains($0) }) else { return false }
    // Absence selects the legacy representation. Null or malformed maps are
    // not absence, and a map must never coexist with a single-pool field.
    guard scope["approved_pools"] != nil else {
        return presentationAddress(scope["pool"]).map({ $0 != zero }) == true
    }
    guard scope["pool"] == nil,
          let pools = scope["approved_pools"] as? [String: Any],
          Set(pools.keys) == Set(tiers.map(String.init)) else { return false }
    return pools.values.allSatisfy { presentationAddress($0).map({ $0 != zero }) == true }
}

private func ammVenueSuffix(_ operation: AMMOperationPresentation) -> String {
    operation.additionalV3 ? " via \(operation.brand)" : ""
}

private func ammStandaloneAuthenticationReason(_ binding: [String: Any], summary: String) -> String? {
    guard let operation = ammOperationPresentation(binding["operation"] as? String ?? ""),
          let execution = binding["execution"] as? [String: Any],
          let intent = binding["financial_intent"] as? [String: Any],
          let token = execution["token"] as? [String: Any],
          let tokenNetwork = presentationString(token["network"], maximum: 64),
          let tokenReference = presentationString(token["reference"], maximum: 42),
          presentationAssetEndpoint(tokenReference),
          let tokenDecimals = presentationInteger(token["decimals"], maximum: 255),
          let unitsAtoms = presentationString(execution["units"], maximum: 100),
          let amount = presentationAtomicQuantity(unitsAtoms, decimals: tokenDecimals),
          let outputAsset = execution["output"] as? [String: Any],
          let outputNetwork = presentationString(outputAsset["network"], maximum: 64),
          outputNetwork == tokenNetwork,
          let outputReference = presentationString(outputAsset["reference"], maximum: 42),
          presentationAssetEndpoint(outputReference),
          let outputDecimals = presentationInteger(outputAsset["decimals"], maximum: 255),
          let minimumAtoms = presentationString(execution["minimum_output"], maximum: 100),
          let minimum = presentationAtomicQuantity(minimumAtoms, decimals: outputDecimals),
          let deadline = presentationInteger(execution["deadline"], minimum: 1),
          let costs = intent["maximum_costs"] as? [[String: Any]],
          costs.count == 1,
          let costAmount = costs[0]["amount"] as? [String: Any],
          let feeAsset = costAmount["asset"] as? [String: Any],
          feeAsset["network"] as? String == tokenNetwork,
          feeAsset["reference"] as? String == "native",
          presentationInteger(feeAsset["decimals"], minimum: 18, maximum: 18) != nil,
          let feeAtoms = presentationString(costAmount["atoms"], maximum: 100),
          let fee = presentationAtomicQuantity(feeAtoms, decimals: 18) else { return nil }
    if operation.additionalV3 {
        guard let transaction = execution["transaction"] as? [String: Any],
              additionalV3Identity(execution, operation: operation, router: transaction["to"]),
              additionalV3PoolScope(execution, operation: operation),
              presentationInteger(transaction["chain_id"], minimum: 1).map({ tokenNetwork == "ethereum:\($0)" }) == true,
              presentationAddress(execution["spender"]) == presentationAddress(transaction["to"]),
              let feeTier = presentationInteger(execution["fee_tier"], minimum: 1, maximum: 10000),
              operation.feeTiers.contains(feeTier) else { return nil }
    }
    let labels = ammLabels(summary: summary, bounded: false,
                           inputReference: tokenReference, outputReference: outputReference)
    let now = Date().timeIntervalSince1970
    guard now.isFinite, deadline > UInt64(max(0, now)) else { return nil }
    let remaining = deadline - UInt64(max(0, now))
    let reason = "allow swapping \(amount) \(labels.input) for at least \(minimum) \(labels.output) on \(reviewedNetworkName(tokenNetwork))\(ammVenueSuffix(operation)); max network fees \(fee) ETH; expires in \(approvalDuration(remaining))."
    let hasPermit = execution["permit_template"].map { !($0 is NSNull) } ?? false
    let scopedPermit = hasPermit
        ? " Any token permit is limited to this swap." : ""
    let completeReason = reason + scopedPermit
    return completeReason.count <= 300 ? completeReason : nil
}

private func ammBoundarySynopsis(_ binding: [String: Any], summary: String) -> String? {
    guard let fixed = binding["fixed"] as? [String: Any],
          let bounds = binding["parameters"] as? [String: Any],
          let operation = fixed["operation"] as? String,
          let venue = ammOperationPresentation(operation),
          binding["operation"] as? String == operation,
          let network = presentationString(fixed["network"], maximum: 64),
          let chainID = presentationInteger(fixed["chain_id"], minimum: 1),
          network == "ethereum:\(chainID)",
          let inputAsset = fixed["token_in"] as? [String: Any],
          let outputAsset = fixed["token_out"] as? [String: Any],
          inputAsset["network"] as? String == network,
          outputAsset["network"] as? String == network,
          let inputToken = presentationString(inputAsset["reference"], maximum: 42),
          let outputToken = presentationString(outputAsset["reference"], maximum: 42),
          presentationAssetEndpoint(inputToken), presentationAssetEndpoint(outputToken),
          let recipient = presentationAddress(fixed["recipient"]),
          recipient == presentationAddress(fixed["signer"]),
          presentationAddress(fixed["router"]) != nil,
          let protocolName = presentationString(fixed["protocol"], maximum: 8),
          (protocolName == "v3" || (operation == "uniswap.swap" && protocolName == "v4")),
          let maxInput = presentationDecimal(bounds["max_input"]),
          let maxFee = presentationDecimal(bounds["max_fee_eth"]),
          let maxSlippage = presentationInteger(bounds["max_slippage_bps"], maximum: 1000),
          let maxImpact = presentationInteger(bounds["max_price_impact_bps"], maximum: 10000),
          let maxDeadline = presentationInteger(bounds["max_deadline_seconds"], minimum: 1, maximum: 600),
          let rate = bounds["minimum_rate"] as? [String: Any],
          let rateInput = presentationString(rate["input_atoms"], maximum: 100),
          let rateOutput = presentationString(rate["output_atoms"], maximum: 100),
          rateInput.range(of: #"^[1-9][0-9]*$"#, options: .regularExpression) != nil,
          rateOutput.range(of: #"^[1-9][0-9]*$"#, options: .regularExpression) != nil,
          let duration = presentationInteger(binding["valid_for_seconds"], minimum: 1) else { return nil }

    guard additionalV3Identity(fixed, operation: venue, router: fixed["router"]),
          additionalV3PoolScope(fixed, operation: venue, bounds: bounds),
          !venue.additionalV3 || absentPresentationValue(bounds["max_pool_fee_ppm"]) else { return nil }
    if operation == "ramses.swap" {
        guard protocolName == "v3",
              let spacing = presentationInteger(fixed["tick_spacing"], minimum: 1, maximum: 200),
              [1, 5, 10, 50, 100, 200].contains(Int(spacing)),
              presentationAddress(fixed["factory"]) != nil,
              presentationAddress(fixed["pool"]) != nil,
              presentationAddress(fixed["pool_deployer"]) != nil,
              fixed["dex"] as? String == "ramses" else { return nil }
        if let tiers = bounds["allowed_fee_tiers"] as? [Any], !tiers.isEmpty { return nil }
        guard presentationInteger(bounds["max_pool_fee_ppm"], maximum: 999_999) != nil else { return nil }
        _ = spacing
    } else if protocolName == "v3" {
        guard let tiers = bounds["allowed_fee_tiers"] as? [Any] else { return nil }
        let parsed = tiers.compactMap { presentationInteger($0, minimum: 1, maximum: 10000) }
        guard !parsed.isEmpty, parsed.count == tiers.count,
              parsed.allSatisfy({ venue.feeTiers.contains($0) }) else { return nil }
    } else {
        guard let tiers = bounds["allowed_fee_tiers"] as? [Any], tiers.isEmpty,
              let poolKey = fixed["v4_pool_key"] as? [String: Any],
              presentationInteger(poolKey["fee"], minimum: 1, maximum: 1_000_000) != nil,
              presentationInteger(poolKey["tick_spacing"], minimum: 1, maximum: 32_767) != nil else { return nil }
    }
    let absolute = bounds["absolute_minimum_output"].flatMap { value -> String? in
        value is NSNull ? nil : presentationDecimal(value)
    }
    if !(bounds["absolute_minimum_output"] == nil || bounds["absolute_minimum_output"] is NSNull || absolute != nil) { return nil }
    let labels = ammLabels(summary: summary, bounded: true,
                           inputReference: inputToken, outputReference: outputToken)
    let networkName = reviewedNetworkName(network)
    let receipt = absolute.map { "for at least \($0) \(labels.output)" } ?? "for \(labels.output)"
    let permitScope = inputToken == "native" ? "" : " A token permit, if needed, is limited to each swap."
    _ = maxSlippage
    _ = maxImpact
    _ = rateInput
    _ = rateOutput
    _ = maxDeadline
    let reason = "allow swapping up to \(maxInput) \(labels.input) \(receipt) on \(networkName)\(ammVenueSuffix(venue)) for \(approvalDuration(duration)); max network fees \(maxFee) ETH.\(permitScope)"
    return reason.count <= 300 ? reason : nil
}

// Exact plans bind their input, schema, connections and accounting scope in
// the broker. They intentionally have no bounded adapter permission spec.
// Check that the displayed exact scope agrees with that frozen plan variant;
// schema/financial enforcement remains in the broker and Rust challenge parser.
private func validExactAMMPlanStep(_ step: [String: Any], permission: [String: Any]) -> Bool {
    guard absentPresentationValue(permission["permission"]),
          let exact = permission["exact"] as? [String: Any],
          let scope = exact["scope"] as? [String: Any],
          scope["mode"] as? String == "exact",
          let variations = step["permitted_variations"] as? [String: Any],
          NSDictionary(dictionary:scope).isEqual(to:variations),
          let operation = step["operation"] as? String,
          let input = step["input"] as? [String: Any] else { return false }
    if !absentPresentationValue(step["action_deadline_seconds"]) {
        return ["uniswap.swap", "ramses.swap"].contains(operation)
            && input["deadline"] == nil
            && presentationInteger(step["action_deadline_seconds"], minimum:1, maximum:600) != nil
    }
    return presentationInteger(input["deadline"], minimum:1) != nil
}

// Validate the reviewed adapter permission against its hash-bound plan step.
// Concise plan prompts still reject malformed or mismatched AMM scopes.
private func validAMMPlanStep(_ step: [String: Any], permission: [String: Any]) -> Bool {
    if !absentPresentationValue(permission["exact"]) {
        return validExactAMMPlanStep(step, permission:permission)
    }
    guard let operation = step["operation"] as? String,
          let venue = ammOperationPresentation(operation),
          let stepID = presentationString(step["step_id"], maximum: 80),
          let input = step["input"] as? [String: Any],
          let permit = permission["permission"] as? [String: Any],
          let spec = permit["spec"] as? [String: Any],
          let fixed = spec["fixed"] as? [String: Any],
          let bounds = spec["parameters"] as? [String: Any],
          presentationString(spec["summary"], maximum: 2048) != nil,
          let inputToken = presentationString(input["token_in"], maximum: 42),
          let outputToken = presentationString(input["token_out"], maximum: 42),
          let inputIdentity = fixed["token_in"] as? [String: Any],
          let outputIdentity = fixed["token_out"] as? [String: Any],
          let inputReference = presentationString(inputIdentity["reference"], maximum: 42),
          let outputReference = presentationString(outputIdentity["reference"], maximum: 42),
          presentationAssetEndpoint(inputToken), presentationAssetEndpoint(outputToken),
          let chainID = presentationInteger(input["chain_id"], minimum: 1),
          inputIdentity["network"] as? String == "ethereum:\(chainID)",
          outputIdentity["network"] as? String == "ethereum:\(chainID)",
          fixed["network"] as? String == "ethereum:\(chainID)",
          presentationInteger(fixed["chain_id"], minimum: 1) == chainID,
          let wallet = presentationString(input["wallet"], maximum: 80),
          fixed["wallet"] as? String == wallet,
          let requestNetwork = presentationString(input["network"], maximum: 16),
          ["evm", "ethereum"].contains(requestNetwork),
          inputToken.lowercased() == inputReference.lowercased(),
          outputToken.lowercased() == outputReference.lowercased(),
          let recipient = presentationAddress(input["recipient"]),
          recipient == presentationAddress(fixed["recipient"]),
          recipient == presentationAddress(fixed["signer"]),
          let router = presentationAddress(fixed["router"]),
          let protocolName = presentationString(fixed["protocol"], maximum: 8),
          (protocolName == "v3" || (operation == "uniswap.swap" && protocolName == "v4")),
          let rawAmount = presentationString(input["amount"], maximum: 100),
          let minimumOutput = presentationDecimal(input["min_output"]),
          let selectedSlippage = presentationInteger(input["slippage_bps"], maximum: 1000),
          let requestFee = presentationDecimal(input["max_fee_eth"]),
          let maxInput = presentationDecimal(bounds["max_input"]),
          let maxFee = presentationDecimal(bounds["max_fee_eth"]),
          let maxSlippage = presentationInteger(bounds["max_slippage_bps"], maximum: 1000),
          let maxImpact = presentationInteger(bounds["max_price_impact_bps"], maximum: 10000),
          let maxDeadline = presentationInteger(bounds["max_deadline_seconds"], minimum: 1, maximum: 600),
          let rate = bounds["minimum_rate"] as? [String: Any],
          let rateInput = presentationString(rate["input_atoms"], maximum: 100),
          let rateOutput = presentationString(rate["output_atoms"], maximum: 100),
          rateInput.range(of: #"^[1-9][0-9]*$"#, options: .regularExpression) != nil,
          rateOutput.range(of: #"^[1-9][0-9]*$"#, options: .regularExpression) != nil else { return false }

    // Relative windows are approved before future-funded steps become eligible.
    // The broker resolves their concrete deadline at preparation, within consent.
    if !absentPresentationValue(step["action_deadline_seconds"]) {
        guard ["uniswap.swap", "ramses.swap"].contains(operation),
              input["deadline"] == nil,
              presentationInteger(step["action_deadline_seconds"], minimum: 1,
                                  maximum: maxDeadline) != nil else { return false }
    } else {
        guard presentationInteger(input["deadline"], minimum: 1) != nil else { return false }
    }

    guard fixed["operation"] as? String == operation,
          additionalV3Identity(fixed, operation: venue, router: fixed["router"]),
          additionalV3PoolScope(fixed, operation: venue, bounds: bounds),
          !venue.additionalV3 || (absentPresentationValue(bounds["max_pool_fee_ppm"])
              && absentPresentationValue(input["max_pool_fee_ppm"])
              && absentPresentationValue(input["tick_spacing"])
              && absentPresentationValue(input["v4_pool_key"])) else { return false }
    if operation == "ramses.swap", fixed["dex"] as? String != "ramses" { return false }

    let requestedProtocol = input["protocol"] == nil ? "v3" : presentationString(input["protocol"], maximum: 8)
    guard requestedProtocol == protocolName else { return false }

    if rawAmount == "all" {
        guard let cap = presentationDecimal(input["max_amount"]), decimalAtMost(cap, maxInput) else { return false }
    } else {
        guard let exact = presentationDecimal(rawAmount), decimalAtMost(exact, maxInput) else { return false }
    }
    guard selectedSlippage <= maxSlippage, decimalAtMost(requestFee, maxFee) else { return false }

    if operation == "ramses.swap" {
        let requestedFeeCap: UInt64?
        if let raw = input["max_pool_fee_ppm"] {
            guard let parsed = presentationInteger(raw, maximum: 999_999) else { return false }
            requestedFeeCap = parsed
        } else {
            requestedFeeCap = nil
        }
        guard protocolName == "v3",
              let spacing = presentationInteger(input["tick_spacing"], minimum: 1, maximum: 200),
              [1, 5, 10, 50, 100, 200].contains(Int(spacing)),
              presentationInteger(fixed["tick_spacing"], minimum: 1, maximum: 200) == spacing,
              let factory = presentationAddress(fixed["factory"]),
              let pool = presentationAddress(fixed["pool"]),
              let deployer = presentationAddress(fixed["pool_deployer"]),
              fixed["dex"] as? String == "ramses" else { return false }
        guard let effectiveCeiling = presentationInteger(bounds["max_pool_fee_ppm"], maximum: 999_999),
              requestedFeeCap.map({ $0 <= effectiveCeiling }) ?? true else { return false }
        _ = spacing
        _ = factory
        _ = pool
        _ = deployer
    } else if protocolName == "v3" {
        guard let allowedTiers = bounds["allowed_fee_tiers"] as? [Any] else { return false }
        let tiers = allowedTiers.compactMap { presentationInteger($0, minimum: 1, maximum: 10_000) }
        guard !tiers.isEmpty, tiers.count == allowedTiers.count,
              tiers.allSatisfy({ venue.feeTiers.contains($0) }) else { return false }
        if input["fee_tier"] != nil {
            guard let selected = presentationInteger(input["fee_tier"], minimum: 1, maximum: 10_000) else { return false }
            guard tiers.contains(selected) else { return false }
        }
    } else {
        guard input["fee_tier"] == nil,
              let allowedTiers = bounds["allowed_fee_tiers"] as? [Any], allowedTiers.isEmpty,
              let poolKey = fixed["v4_pool_key"] as? [String: Any],
              let poolFee = presentationInteger(poolKey["fee"], minimum: 1, maximum: 1_000_000),
              let spacing = presentationInteger(poolKey["tick_spacing"], minimum: 1, maximum: 32_767) else { return false }
        _ = poolFee
        _ = spacing
    }

    let floor = bounds["absolute_minimum_output"].flatMap { value -> String? in
        value is NSNull ? nil : presentationDecimal(value)
    }
    if !(bounds["absolute_minimum_output"] == nil || bounds["absolute_minimum_output"] is NSNull || floor != nil) { return false }
    _ = stepID
    _ = recipient
    _ = router
    _ = minimumOutput // This is an initial request, not a floor for bounded requotes.
    _ = selectedSlippage
    _ = maxSlippage
    _ = maxImpact
    _ = rateInput
    _ = rateOutput
    return true
}

private func presentationAtomicQuantity(_ atoms: String, decimals: UInt64) -> String? {
    guard atoms.range(of: #"^[0-9]+$"#, options: .regularExpression) != nil,
          decimals <= 255 else { return nil }
    let significant = String(atoms.drop(while: { $0 == "0" }))
    let digits = significant.isEmpty ? "0" : significant
    guard decimals > 0 else { return digits }
    let width = Int(decimals)
    let padded = String(repeating: "0", count: max(0, width + 1 - digits.count)) + digits
    let split = padded.index(padded.endIndex, offsetBy: -width)
    let wholeDigits = String(padded[..<split]).drop(while: { $0 == "0" })
    let whole = wholeDigits.isEmpty ? "0" : String(wholeDigits)
    let fraction = String(padded[split...]).reversed().drop(while: { $0 == "0" }).reversed()
    return fraction.isEmpty ? whole : "\(whole).\(String(fraction))"
}

private func validPlanAggregateBounds(_ plan: [String: Any]) -> Bool {
    guard let aggregate = plan["aggregate_bounds"] as? [String: Any],
          let debits = aggregate["maximum_debits"] as? [[String: Any]],
          let costs = aggregate["maximum_costs"] as? [[String: Any]],
          !debits.isEmpty, !costs.isEmpty else { return false }
    func validAmount(_ value: Any?) -> Bool {
        guard let amount = value as? [String: Any],
              let atoms = presentationString(amount["atoms"], maximum: 100),
              atoms.range(of: #"^[0-9]+$"#, options: .regularExpression) != nil,
              let asset = amount["asset"] as? [String: Any],
              presentationString(asset["network"], maximum: 64) != nil,
              presentationString(asset["reference"], maximum: 64) != nil,
              let decimals = presentationInteger(asset["decimals"], maximum: 255),
              presentationAtomicQuantity(atoms, decimals: decimals) != nil else { return false }
        return true
    }
    return debits.allSatisfy { row in
        row["resource"] is String && validAmount(row["amount"])
    } && costs.allSatisfy { row in
        guard let location = row["location"] as? [String: Any],
              let kind = presentationString(location["kind"], maximum: 32),
              ["cash", "position"].contains(kind),
              location["resource"] is String else { return false }
        return validAmount(row["amount"])
    }
}

// Every named revision uses the same concise prompt, independent of its adapters.
// Read the caller-authored name/summary and the step count/duration from the
// verified approval binding. Display text does not grant financial authority;
// the broker independently enforces the complete plan's structured limits.
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
    var planSummary: String?
    if let supplied = plan["summary"] {
        guard let text = supplied as? String, !text.isEmpty,
              text.trimmingCharacters(in: .whitespacesAndNewlines) == text,
              name.unicodeScalars.count + text.unicodeScalars.count <= 140,
              text.unicodeScalars.allSatisfy({ scalar in
                  let value = scalar.value
                  return value > 0x1f && !(0x7f...0x9f).contains(value)
                      && value != 0x2028 && value != 0x2029
                      && value != 0x061c && value != 0x200e && value != 0x200f
                      && !(0x202a...0x202e).contains(value)
                      && !(0x2066...0x2069).contains(value)
              }) else { return nil }
        planSummary = text
    }
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
    // New summaries must fit in full with their name and approval limits.
    // Existing plans without a summary retain their original presentation.
    guard lead.unicodeScalars.count <= authenticationReasonMaximumCharacters else { return nil }
    let durationSeconds = duration.uint64Value
    let details = " (\(steps.count) \(steps.count == 1 ? "step" : "steps"), \(approvalDuration(durationSeconds)))"
    let detailed = lead + (planSummary.map { ". " + $0 } ?? "") + details
    let ammSteps = zip(steps, permissions).filter { step, _ in
        ammOperationPresentation(step["operation"] as? String ?? "") != nil
    }
    if !ammSteps.isEmpty {
        guard ammSteps.allSatisfy({ validAMMPlanStep($0.0, permission: $0.1) }),
              validPlanAggregateBounds(plan),
              summary == "Approve the plan “\(name)” with \(steps.count) ordered steps for \(duration.uint64Value) seconds." else { return nil }
    }
    if planSummary != nil {
        return detailed.unicodeScalars.count <= authenticationReasonMaximumCharacters ? detailed : nil
    }
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
    if action == "configure" {
        guard let operation = binding["operation"] as? String, let account = binding["account"] as? String,
              let product = binding["product"] as? String, ["kraken", "krakenfutures"].contains(product),
              operation.hasPrefix(product + "."),
              !account.isEmpty, account.count <= 96, binding["parameters"] is [String: Any] else { return nil }
        return compactAuthenticationReason(prefix: "configure ", summary: "\(operation) for account \(account)")
    }
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
    case "kraken": providerName = "Kraken"
    case "krakenfutures": providerName = "Kraken Futures"
    case "solana": providerName = "Solana"
    case "jupiter": providerName = "Jupiter"
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
          let action = binding["action"] as? String, ["initialize", "add", "update", "remove", "policy_set", "configure"].contains(action),
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
