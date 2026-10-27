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

// Keep malformed financial reviews distinct from configuration review failures.
// This only selects the failure status; it never grants approval or bypasses validation.
func isFinancialApprovalBinding(_ binding: [String: Any]) -> Bool {
    if let kind = binding["kind"] as? String,
       ["operation_boundaries", "financial_plan"].contains(kind) { return true }
    return binding["action"] == nil && (binding["operation"] != nil
        || binding["execution"] != nil || binding["financial_intent"] != nil)
}

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

// The override prompt names the original operation and states the continuing
// execution risk. It never claims that releasing a lock cancels a submission.
func reservationOverrideAuthenticationReason(_ binding: [String: Any]) -> String? {
    guard Set(binding.keys) == Set(["action", "version", "request_id", "review_hash", "review"]),
          binding["action"] as? String == "reservation_override",
          presentationInteger(binding["version"], minimum: 1, maximum: 1) == 1,
          presentationString(binding["request_id"]) != nil,
          let reviewHash = presentationString(binding["review_hash"], maximum: 64),
          reviewHash.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil,
          let review = binding["review"] as? [String: Any],
          presentationInteger(review["version"], minimum: 1, maximum: 1) == 1,
          let operationID = presentationString(review["operation_id"], maximum: 128),
          operationID.range(of: "^[A-Za-z0-9._:-]+$", options: .regularExpression) != nil,
          let operation = presentationString(review["operation"], maximum: 128),
          operation.range(of: "^[A-Za-z0-9._:-]+$", options: .regularExpression) != nil,
          let resources = review["resources"] as? [String], !resources.isEmpty,
          Set(resources).count == resources.count,
          resources.allSatisfy({ presentationString($0) != nil }),
          let recordHash = presentationString(review["record_hash"], maximum: 64),
          recordHash.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil,
          let reservationsHash = presentationString(review["reservations_hash"], maximum: 64),
          reservationsHash.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil,
          let released = review["released_reservations"] as? [[String: Any]], !released.isEmpty,
          released.allSatisfy({ row in
              Set(row.keys) == Set(["resource", "units", "fee_units"]) &&
              presentationString(row["resource"]) != nil &&
              [row["units"], row["fee_units"]].allSatisfy({ value in
                  guard let quantity = value as? String else { return false }
                  return quantity.range(of: "^(0|[1-9][0-9]*)$", options: .regularExpression) != nil
              })
          }),
          let cash = review["released_cash"] as? [[String: Any]],
          review["remaining_reservations"] is [[String: Any]],
          review["submission"] is [String: Any],
          presentationString(review["warning"], maximum: 1024) != nil else { return nil }
    // Never derive the risk statement from caller prose or shorten the original
    // operation identity. The canonical broker review binds exact resources.
    let fundedRows = released.filter { ($0["units"] as? String) != "0" || ($0["fee_units"] as? String) != "0" }
    // The engine aggregates legacy cost and principal rows for the same exact
    // cash identity, so their raw row count need not equal this typed count.
    guard cash.isEmpty == fundedRows.isEmpty else { return nil }
    var amounts: [String] = []
    var cashIdentities = Set<String>()
    for row in cash {
        guard Set(row.keys) == Set(["resource", "asset", "amount_atoms"]),
              let resource = row["resource"] as? String, resources.contains(resource),
              let asset = row["asset"] as? [String: Any],
              Set(asset.keys) == Set(["network", "reference", "decimals"]),
              let network = presentationString(asset["network"], maximum: 128),
              network.range(of: "^[A-Za-z0-9._:-]+$", options: .regularExpression) != nil,
              let reference = presentationString(asset["reference"], maximum: 128),
              reference.range(of: "^[A-Za-z0-9._:-]+$", options: .regularExpression) != nil,
              let decimals = presentationInteger(asset["decimals"], maximum: 255),
              let atoms = presentationString(row["amount_atoms"], maximum: 100),
              atoms.range(of: "^[1-9][0-9]*$", options: .regularExpression) != nil,
              let amount = presentationAtomicQuantity(atoms, decimals: decimals),
              cashIdentities.insert("\(resource)|\(network)|\(reference)|\(decimals)").inserted else { return nil }
        amounts.append("\(amount) \(reference)@\(network)")
    }
    let cashScope = amounts.isEmpty ? "" : "\(amounts.joined(separator: ", ")) (incl. fees); "
    let scope = "\(cashScope)\(resources.count) account \(resources.count == 1 ? "lock" : "locks")"
    let reason = "release \(scope) for \(operation) (\(operationID)). The original action may still execute, risking double use. This does not cancel it."
    return reason.unicodeScalars.count <= authenticationReasonMaximumCharacters ? reason : nil
}

// Resolution is a human attestation, not a venue proof. Its longer, separate
// budget preserves every amount; callers reject overflow instead of truncating.
let operationResolutionReasonMaximumCharacters = 1024
func operationResolutionAuthenticationReason(_ binding: [String: Any]) -> String? {
    func hash(_ value: Any?) -> Bool {
        guard let text = value as? String else { return false }
        return text.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil
    }
    func identifier(_ value: Any?) -> String? {
        guard let text = presentationString(value, maximum:256),
              text.range(of:"^[A-Za-z0-9._:-]+$", options:.regularExpression) != nil else { return nil }
        return text
    }
    func amount(_ value: Any?) -> String? {
        guard let row = value as? [String:Any], Set(row.keys) == Set(["asset","atoms"]),
              let asset = row["asset"] as? [String:Any], Set(asset.keys) == Set(["network","reference","decimals"]),
              let network = identifier(asset["network"]), let reference = identifier(asset["reference"]),
              let decimals = presentationInteger(asset["decimals"], maximum:255),
              let atoms = presentationString(row["atoms"], maximum:256),
              atoms.range(of:"^(0|[1-9][0-9]*)$", options:.regularExpression) != nil,
              let quantity = presentationAtomicQuantity(atoms, decimals:decimals) else { return nil }
        return "\(quantity) \(reference)@\(network)"
    }
    // Reservation rows use the engine's serialized tuple keys, not display
    // resource IDs. Accept only the exact typed cash/cost formats it produces.
    func reservationKey(_ value: Any?) -> Bool {
        guard let key = presentationString(value, maximum:4096) else { return false }
        if identifier(key) != nil { return true }
        guard let data = key.data(using:.utf8),
              let tuple = try? JSONSerialization.jsonObject(with:data) as? [Any],
              tuple.count == 2 || tuple.count == 3,
              let asset = tuple[1] as? [String:Any],
              amount(["asset":asset,"atoms":"0"]) != nil,
              let network = identifier(asset["network"]), let reference = identifier(asset["reference"]),
              let decimals = presentationInteger(asset["decimals"],maximum:255) else { return false }
        let assetKey = "{\"network\":\"\(network)\",\"reference\":\"\(reference)\",\"decimals\":\(decimals)}"
        if tuple.count == 2, let resource = identifier(tuple[0]) {
            return key == "[\"\(resource)\",\(assetKey)]"
        }
        guard tuple.count == 3, let location = tuple[0] as? [String:Any],
              let resource = identifier(location["resource"]), let treatment = tuple[2] as? String,
              ["additional_debit","venue_calculated_additional_debit","included_in_debit","deducted_from_credit"].contains(treatment) else { return false }
        let locationKey:String
        if location["kind"] as? String == "cash" && Set(location.keys) == Set(["kind","resource"]) {
            locationKey = "{\"kind\":\"cash\",\"resource\":\"\(resource)\"}"
        } else if location["kind"] as? String == "position" && Set(location.keys) == Set(["kind","resource","position","position_kind"]),
                  let position = identifier(location["position"]), let kind = identifier(location["position_kind"]) {
            locationKey = "{\"kind\":\"position\",\"resource\":\"\(resource)\",\"position\":\"\(position)\",\"position_kind\":\"\(kind)\"}"
        } else { return false }
        return key == "[\(locationKey),\(assetKey),\"\(treatment)\"]"
    }
    func resources(_ value: Any?) -> [String]? {
        guard let rows = value as? [[String:Any]] else { return nil }
        var rendered:[String] = []
        for row in rows {
            guard Set(row.keys) == Set(["resource","amount"]), let resource = identifier(row["resource"]),
                  let quantity = amount(row["amount"]) else { return nil }
            rendered.append("\(quantity) [\(resource)]")
        }
        return rendered
    }
    guard Set(binding.keys) == Set(["action","version","request_id","review_hash","review"]),
          binding["action"] as? String == "operation_resolution",
          presentationInteger(binding["version"], minimum:1, maximum:1) == 1,
          identifier(binding["request_id"]) != nil, hash(binding["review_hash"]),
          let review = binding["review"] as? [String:Any],
          presentationInteger(review["version"], minimum:1, maximum:1) == 1,
          let operationID = identifier(review["operation_id"]), let operation = identifier(review["operation"]),
          identifier(review["original_request_id"]) != nil, identifier(review["fingerprint"]) != nil,
          ["record_hash","reservations_hash"].allSatisfy({ hash(review[$0]) }),
          review["plan_hash"] is NSNull || hash(review["plan_hash"]),
          review["submission"] is [String:Any], let impact = review["impact"] as? [String:Any],
          let capBreach = impact["historical_cap_breach"] as? Bool,
          impact["future_authority_increased"] as? Bool == false,
          impact["allocation_charges_retained"] as? Bool == true,
          impact["plan_usage_retained"] as? Bool == true,
          impact["receipt_provenance"] as? String == "human_attested",
          impact["plan_linked"] as? Bool == !(review["plan_hash"] is NSNull),
          let warnings = review["warnings"] as? [String], !warnings.isEmpty,
          warnings.allSatisfy({ presentationString($0, maximum:2048) != nil }),
          let accountResources = review["resources"] as? [String], accountResources.allSatisfy({identifier($0) != nil}),
          let releasedRows = review["released_reservations"] as? [[String:Any]],
          let remainingRows = review["remaining_reservations"] as? [[String:Any]],
          (releasedRows + remainingRows).allSatisfy({ row in
              Set(row.keys) == Set(["resource","units","fee_units"]) && reservationKey(row["resource"]) &&
              [row["units"],row["fee_units"]].allSatisfy({ value in
                  guard let text = value as? String else { return false }
                  return text.range(of:"^(0|[1-9][0-9]*)$",options:.regularExpression) != nil
              })
          }),
          let attestation = review["attestation"] as? [String:Any],
          Set(attestation.keys) == Set(["outcome","reason","actual_debits","actual_costs","reported_receipts"]),
          let outcome = attestation["outcome"] as? String, ["succeeded","failed"].contains(outcome),
          let explanation = presentationString(attestation["reason"], maximum:1024),
          let debits = resources(attestation["actual_debits"]), let receipts = resources(attestation["reported_receipts"]),
          let costs = attestation["actual_costs"] as? [[String:Any]],
          let releasedCash = review["released_cash"] as? [[String:Any]] else { return nil }
    var fees:[String] = []
    for cost in costs {
        guard Set(cost.keys) == Set(["location","amount","treatment"]),
              let quantity = amount(cost["amount"]), let location = cost["location"] as? [String:Any],
              let resource = identifier(location["resource"]),
              let treatment = cost["treatment"] as? String,
              ["additional_debit","venue_calculated_additional_debit","included_in_debit","deducted_from_credit"].contains(treatment) else { return nil }
        let locationLabel:String
        if location["kind"] as? String == "cash" && Set(location.keys) == Set(["kind","resource"]) {
            locationLabel = resource
        } else if location["kind"] as? String == "position" && Set(location.keys) == Set(["kind","resource","position","position_kind"]),
                  let position = identifier(location["position"]), let kind = identifier(location["position_kind"]) {
            locationLabel = "\(resource)/\(kind)/\(position)"
        } else { return nil }
        fees.append("\(quantity) [\(locationLabel); \(treatment.replacingOccurrences(of:"_",with:" "))]")
    }
    var holds:[String] = []
    for cash in releasedCash {
        guard Set(cash.keys) == Set(["resource","asset","amount_atoms"]),
              let resource = identifier(cash["resource"]),
              let quantity = amount(["asset":cash["asset"] as Any,"atoms":cash["amount_atoms"] as Any]) else { return nil }
        holds.append("\(quantity) [\(resource)]")
    }
    func list(_ rows:[String]) -> String { rows.isEmpty ? "none" : rows.joined(separator:", ") }
    let retained = remainingRows.isEmpty ? "" : " \(remainingRows.count) holds remain; their resources stay blocked."
    return "attest \(operation) (\(operationID)) \(outcome). Debits: \(list(debits)); costs: \(list(fees)); receipts: \(list(receipts)); release holds: \(list(holds)) (\(releasedRows.count) reservations). Reason: \(explanation). Human-attested, not externally verified. Original may still act; no resubmission. Reported receipts may fund next steps. Historical budget charges remain; future limits unchanged.\(retained)\(capBreach ? " Reported amounts exceed original caps." : "")"
}

enum OperationResolutionPresentationFailure: String, Error {
    case invalidReview = "invalid_resolution_review"
    case tooLarge = "review_too_large"
}

// Shared by the helper and synthetic tests: never mistake a rendering failure
// for rejection or cancellation by the human, and never truncate the review.
func operationResolutionApprovalReason(_ binding: [String: Any]) -> Result<String, OperationResolutionPresentationFailure> {
    guard let reason = operationResolutionAuthenticationReason(binding) else { return .failure(.invalidReview) }
    guard reason.unicodeScalars.count <= operationResolutionReasonMaximumCharacters else { return .failure(.tooLarge) }
    return .success(reason)
}

// Every named revision uses the same concise prompt, independent of its adapters.
// Read the caller-authored name/summary and the step count/duration from the
// verified approval binding. Display text does not grant financial authority;
// the broker independently enforces the complete plan's structured limits.
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
    case "coinbase", "coinbaseexchange", "coinbaseinternational":
        let baseKeys: Set<String> = ["provider", "name", "resource", "credential_ref", "physical_identity", "physical_identity_verified"]
        let connectionKeys = Set(connection.keys)
        guard
            connectionKeys == baseKeys || connectionKeys == baseKeys.union(["portfolio_id"]),
            let identity = connection["physical_identity"] as? String,
            !identity.isEmpty,
            identity.count <= 128,
            identity.utf8.allSatisfy({ ($0 >= 48 && $0 <= 57) || ($0 >= 65 && $0 <= 90) || ($0 >= 97 && $0 <= 122) || $0 == 95 || $0 == 45 }),
            let identityVerified = connection["physical_identity_verified"] as? NSNumber,
            CFGetTypeID(identityVerified) == CFBooleanGetTypeID(), identityVerified.boolValue,
            let credentialReference = connection["credential_ref"] as? String, !credentialReference.isEmpty,
            connection["resource"] as? String == "\(provider):portfolio:\(identity)",
            connection["portfolio_id"] == nil || connection["portfolio_id"] as? String == identity else { return nil }
        switch provider {
        case "coinbase": providerName = "Coinbase"
        case "coinbaseexchange": providerName = "Coinbase Exchange"
        default: providerName = "Coinbase International"
        }
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
    if action == "remove" { lines.append("GoFin will remove the saved connection and try to delete its credentials. External accounts, funds and positions remain; any credential cleanup failure will be reported.") }
    let visibleWarnings = warnings.map { "Warning: \($0)" }
    return ConfigurationPresentation(title: "\(verb) GoFin connection", body: (visibleWarnings + lines + [footer]).joined(separator: "\n\n"))
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

// The broker creates this typed review and hashes it together with the execution
// binding. The helper renders only that review; it never interprets venue data.
enum FinancialReviewFailure: String, Error {
    case invalid = "invalid_financial_review"
    case tooLong = "financial_review_too_long"
}

func typedFinancialApprovalReason(_ binding: [String: Any]) -> Result<String, FinancialReviewFailure> {
    guard presentationInteger(binding["version"], minimum: 1, maximum: 1) == 1,
          binding["kind"] as? String == "financial_approval_review",
          let purpose = binding["purpose"] as? String, ["financial_write", "financial_boundary", "financial_plan"].contains(purpose),
          binding["execution_binding"] is [String: Any],
          let review = binding["review"] as? [String: Any],
          presentationInteger(review["version"], minimum: 2, maximum: 2) == 2,
          let action = review["action"] as? String else { return .failure(.invalid) }
    func safe(_ value: Any?, maximum: Int = 240) -> String? {
        guard let s = value as? String, !s.isEmpty, s.count <= maximum,
              s.trimmingCharacters(in: .whitespacesAndNewlines) == s,
              s.unicodeScalars.allSatisfy({ scalar in
                  let v = scalar.value
                  return (!CharacterSet.controlCharacters.contains(scalar) || v == 0x200d) && v != 0x2028 && v != 0x2029
                      && v != 0x061c && v != 0x200e && v != 0x200f
                      && !(0x202a...0x202e).contains(v) && !(0x2066...0x2069).contains(v)
              }) else { return nil }
        return s
    }
    func optionalString(_ object: [String: Any], _ key: String, maximum: Int = 240) -> (Bool, String?) {
        guard let value = object[key], !(value is NSNull) else { return (true, nil) }
        guard let result = safe(value, maximum: maximum) else { return (false, nil) }
        return (true, result)
    }
    guard Set(review.keys).isSubset(of: Set(["version", "action", "service", "market", "input", "output", "other_details", "description", "step_count", "valid_for_seconds"])),
          let service = safe(review["service"]) else { return .failure(.invalid) }
    if let details = review["other_details"] as? String, details.count > 200 { return .failure(.tooLong) }
    let (marketOK, market) = optionalString(review, "market")
    let (detailsOK, details) = (review["other_details"] as? String == "") ? (true, nil) : optionalString(review, "other_details", maximum: 200)
    let (descriptionOK, description) = optionalString(review, "description")
    guard marketOK, detailsOK, descriptionOK else { return .failure(.invalid) }
    func side(_ key: String) -> (Bool, String?) {
        guard let raw = review[key], !(raw is NSNull) else { return (true, nil) }
        guard let object = raw as? [String: Any], Set(object.keys).isSubset(of: Set(["amount", "asset", "account"])) else { return (false, nil) }
        let (amountOK, amount) = optionalString(object, "amount")
        var asset: String?
        if let rawAsset = object["asset"], !(rawAsset is NSNull) {
            guard let a = rawAsset as? [String: Any], Set(a.keys) == Set(["name", "identifier"]),
                  let name = safe(a["name"]), let id = a["identifier"] as? [String: Any],
                  Set(id.keys) == Set(["network", "reference", "decimals"]),
                  safe(id["network"], maximum: Int.max) != nil,
                  safe(id["reference"], maximum: Int.max) != nil,
                  presentationInteger(id["decimals"], maximum: 255) != nil else { return (false, nil) }
            asset = name
        }
        let assetOK = true
        guard amountOK, assetOK, amount == nil || amount!.range(of: #"^(0|[1-9][0-9]*)(\.[0-9]+)?$"#, options: .regularExpression) != nil else { return (false, nil) }
        var account: String?
        if let rawAccount = object["account"], !(rawAccount is NSNull) {
            guard let a = rawAccount as? [String: Any], Set(a.keys).isSubset(of: Set(["name", "domain"])), let name = safe(a["name"]) else { return (false, nil) }
            let (domainOK, domain) = optionalString(a, "domain")
            guard domainOK else { return (false, nil) }
            account = domain.map { "\(name) (\($0))" } ?? name
        }
        let parts = [amount, asset].compactMap { $0 }.joined(separator: " ")
        if parts.isEmpty && account == nil { return (true, nil) }
        return (true, [parts.isEmpty ? nil : parts, account].compactMap { $0 }.joined(separator: " at "))
    }
    let (inputOK, input) = side("input"), (outputOK, output) = side("output")
    guard inputOK, outputOK else { return .failure(.invalid) }
    let verbs: [String: String] = [
        "swap":"swap", "account-transfer":"transfer", "exchange-withdrawal":"withdraw", "exchange-deposit":"deposit",
        "cancel-order":"cancel order", "cancel-withdrawal":"cancel withdrawal", "authorize-boundaries":"authorize", "lending-borrow":"borrow", "lending-deposit":"supply", "lending-withdraw":"withdraw",
        "vault-deposit":"deposit into vault", "vault-withdraw":"withdraw from vault", "set-leverage":"set leverage",
        "lending-repay":"repay", "collect-fees":"collect fees", "execute-plan":"execute plan", "bridge":"bridge",
        "place-order":"place order", "edit-order":"edit order", "batch-orders":"place order batch", "schedule-cancel":"schedule cancellation", "approve":"approve", "recovery":"recover",
        "configure-collateral":"configure collateral", "set-margin-type":"set margin type", "add-margin":"add margin", "reduce-margin":"reduce margin",
        "configure-account":"configure account", "mint":"mint", "burn":"burn", "open-position":"open position", "close-position":"close position", "increase-position":"increase position", "decrease-position":"decrease position",
        "add-liquidity":"add liquidity", "remove-liquidity":"remove liquidity", "claim":"claim", "stake":"stake", "unstake":"unstake",
        "wrap":"wrap", "unwrap":"unwrap", "allocate":"allocate", "deallocate":"deallocate"
    ]
    guard let verb = verbs[action] else { return .failure(.invalid) }
    func has(_ side: String, _ key: String) -> Bool {
        guard let value = (review[side] as? [String: Any])?[key] else { return false }
        return !(value is NSNull)
    }
    guard (purpose == "financial_boundary") == (action == "authorize-boundaries"),
          (purpose == "financial_plan") == (action == "execute-plan") else { return .failure(.invalid) }
    if action == "swap" {
        guard has("input", "amount"), has("input", "asset"), has("output", "asset"), has("output", "amount") else { return .failure(.invalid) }
    }
    if ["account-transfer", "bridge", "exchange-withdrawal", "exchange-deposit"].contains(action) {
        guard has("input", "account"), has("output", "account"),
              (has("input", "amount") && has("input", "asset") || has("output", "amount") && has("output", "asset")) else { return .failure(.invalid) }
    }
    if ["lending-borrow", "lending-withdraw", "vault-withdraw"].contains(action), !has("output", "amount") || !has("output", "asset") { return .failure(.invalid) }
    if ["lending-deposit", "vault-deposit", "lending-repay"].contains(action), !has("input", "amount") || !has("input", "asset") { return .failure(.invalid) }
    if ["cancel-order", "cancel-withdrawal", "set-leverage", "place-order", "edit-order", "batch-orders", "schedule-cancel", "configure-collateral", "set-margin-type", "configure-account", "recovery"].contains(action), market == nil && details == nil { return .failure(.invalid) }
    func quantity(_ side: String) -> String? {
        guard let value = review[side] as? [String: Any] else { return nil }
        let parts = [value["amount"] as? String, (value["asset"] as? [String: Any])?["name"] as? String].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " ")
    }
    func completeQuantity(_ side: String) -> String? {
        guard has(side, "amount"), has(side, "asset") else { return nil }
        return quantity(side)
    }
    func account(_ side: String) -> String? {
        guard let value = (review[side] as? [String: Any])?["account"] as? [String: Any],
              let name = value["name"] as? String else { return nil }
        return (value["domain"] as? String).map { "\(name) (\($0))" } ?? name
    }
    let location = "on \(service)" + (market.map { " in \($0)" } ?? "")
    var result: String
    switch action {
    case "swap":
        result = "swap \(quantity("input")!) for at least \(quantity("output")!) \(location)"
        if let a = account("input") { result += " via \(a)" }
        if let a = account("output"), a != account("input") { result += " to \(a)" }
    case "account-transfer", "bridge", "exchange-withdrawal", "exchange-deposit":
        let amount = completeQuantity("input") ?? completeQuantity("output")!
        result = "\(verb) \(amount) from \(account("input")!) to \(account("output")!) \(location)"
        if let outputQuantity = completeQuantity("output") { result += "; at least \(outputQuantity)" }
        if completeQuantity("input") == nil, let q = quantity("input"), q != quantity("output") { result += "; input: \(q)" }
        if completeQuantity("output") == nil, let q = quantity("output"), q != quantity("input") { result += "; output: \(q)" }
    case "lending-borrow":
        result = "borrow" + ((quantity("output") ?? quantity("input")).map { " \($0)" } ?? "") + " \(location)"
        if let a = account("output") { result += " to \(a)" }
        if let a = account("input"), a != account("output") { result += " from \(a)" }
        if let q = quantity("input"), q != quantity("output") { result += "; input: \(q)" }
    case "lending-withdraw", "vault-withdraw":
        result = "\(verb) at least \(quantity("output")!) \(location)"
        if let a = account("input") { result += " from \(a)" }
        if let a = account("output"), a != account("input") { result += " to \(a)" }
        if let q = quantity("input"), q != quantity("output") { result += "; input: \(q)" }
    case "authorize-boundaries":
        result = "authorize" + (market.map { " \($0)" } ?? " actions") + " on \(service)"
        if let q = quantity("input") { result += " with up to \(q)" }
        if let a = account("input") { result += " via \(a)" }
        if let q = quantity("output") { result += "; output: \(q)" }
        if let a = account("output"), a != account("input") { result += " to \(a)" }
    case "execute-plan":
        guard let description = description, let count = presentationInteger(review["step_count"], minimum: 1),
              let duration = presentationInteger(review["valid_for_seconds"], minimum: 1) else { return .failure(.invalid) }
        let period = duration % 60 == 0 ? "\(duration / 60) min" : "\(duration) seconds"
        result = "approve \(count) steps for \(period): \(description) on \(service)"
        if let market = market { result += " in \(market)" }
        if let input = input { result += "; input: \(input)" }
        if let output = output { result += "; output: \(output)" }
    default:
        result = verb + ((quantity("input") ?? quantity("output")).map { " \($0)" } ?? "") + " \(location)"
        if let a = account("input") { result += " from \(a)" }
        if let a = account("output"), a != account("input") { result += " to \(a)" }
        if let outputQuantity = quantity("output"), quantity("input") != nil, outputQuantity != quantity("input") { result += "; at least \(outputQuantity)" }
    }
    if action != "execute-plan" && (description != nil || (review["step_count"] != nil && !(review["step_count"] is NSNull)) || (review["valid_for_seconds"] != nil && !(review["valid_for_seconds"] is NSNull))) { return .failure(.invalid) }
    if let details = details { result += "; \(details)" }
    guard result.count <= authenticationReasonMaximumCharacters else { return .failure(.tooLong) }
    return .success(result)
}
