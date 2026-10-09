import Foundation

@main
struct ApprovalPresentationTests {
    static func main() {
        let connection: [String: Any] = [
            "provider": "binance", "name": "SYNTHETIC-main", "resource": "binance:uid:10001:SPOT",
            "credential_ref": "SYNTHETIC-REFERENCE", "role": "main", "parent": NSNull(), "email": NSNull(),
            "wallet_type": "SPOT", "physical_identity": "10001", "physical_identity_verified": true,
        ]
        let checks: [[String: Any]] = [
            ["name": "credential authentication", "status": "passed"],
            ["name": "physical account identity", "status": "passed", "uid": "10001", "source": "GET /api/v3/account"],
            ["name": "reported API restrictions", "status": "observed", "permissions": [
                "enableReading": true, "enableWithdrawals": false, "enableInternalTransfer": true,
                "permitsUniversalTransfer": false, "ipRestrict": true, "enableSpotAndMarginTrading": NSNull(),
            ]],
        ]
        let binding: [String: Any] = [
            "action": "add", "connection": connection, "checks": checks, "warnings": [String](),
            "configuration_hash": String(repeating: "a", count: 64), "financial_movement": false,
            "future_financial_authority": false,
        ]
        let main = configurationPresentation(binding)!
        assert(main.title == "Save Fin connection")
        for text in ["SYNTHETIC-main", "UID 10001", "Enabled: Reading, Internal transfers, IP restriction",
                     "Disabled: Withdrawals, Universal transfers", "Unknown: Spot / margin trading",
                     "No funds move", "fresh Touch ID", "financial writes untested"] { assert(main.body.contains(text), text) }
        assert(!main.body.contains("SYNTHETIC-REFERENCE"))
        assert(main.body.components(separatedBy: "\n").count <= 12)

        var sub = binding
        var child = connection
        child["role"] = "sub"; child["parent"] = "SYNTHETIC-parent"; child["email"] = "synthetic@example.invalid"
        sub["connection"] = child
        sub["checks"] = checks + [
            ["name": "parent child email", "status": "passed", "email": "synthetic@example.invalid"],
            ["name": "child UID/email association", "status": "human_confirmation_required", "uid": "10001", "email": "synthetic@example.invalid"],
        ]
        sub["warnings"] = ["SYNTHETIC visible warning"]
        let childSummary = configurationPresentation(sub)!.body
        assert(!childSummary.contains("Verify in Binance") && !childSummary.contains("UID association"))
        for text in ["UID 10001", "Child email: synthetic@example.invalid", "SYNTHETIC-parent", "SYNTHETIC visible warning"] { assert(childSummary.contains(text)) }
        var childUpdate = sub; childUpdate["action"] = "update"
        let childUpdateSummary = configurationPresentation(childUpdate)!.body
        assert(!childUpdateSummary.contains("Verify in Binance") && !childUpdateSummary.contains("UID association"))
        assert(childUpdateSummary.contains("Child email: synthetic@example.invalid") && childUpdateSummary.contains("UID 10001"))
        var missingAssociation = sub; missingAssociation["checks"] = checks
        assert(configurationPresentation(missingAssociation) == nil)

        var update = binding; update["action"] = "update"
        assert(configurationPresentation(update)!.body.contains("old credentials will be replaced"))
        var remove = binding; remove["action"] = "remove"; remove.removeValue(forKey: "checks"); remove.removeValue(forKey: "warnings")
        remove["warning"] = "Keep independent recovery credentials for remaining funds."
        let removal = configurationPresentation(remove)!.body
        assert(removal.hasPrefix("Warning: Keep independent recovery"))
        assert(removal.contains("External accounts, funds and positions remain"))

        for mutation in ["missing", "unsupported", "financial", "unknown_field", "unknown_check", "changed_uid", "control_text"] {
            var bad = binding
            switch mutation {
            case "missing": bad.removeValue(forKey: "checks")
            case "unsupported": bad["action"] = "initialize"
            case "financial": bad["financial_movement"] = true
            case "unknown_field": bad["new_authority"] = true
            case "unknown_check": bad["checks"] = checks + [["name": "new validation", "status": "passed"]]
            case "changed_uid": var value = connection; value["physical_identity"] = "99999"; bad["connection"] = value
            default: var value = connection; value["name"] = "name\nInjected claim"; bad["connection"] = value
            }
            assert(configurationPresentation(bad) == nil, mutation)
        }
        assert(configurationPresentation(["chain_id": 1, "amount_atoms": "10"]) == nil)
        var unknownPermission = binding
        var alteredChecks = checks
        alteredChecks[2]["permissions"] = ["unexpectedPermission": true]
        unknownPermission["checks"] = alteredChecks
        assert(configurationPresentation(unknownPermission) == nil)
        alteredChecks[2]["permissions"] = ["enableReading": 1]
        unknownPermission["checks"] = alteredChecks
        assert(configurationPresentation(unknownPermission) == nil)
        alteredChecks[2]["permissions"] = [:] as [String: Any]
        unknownPermission["checks"] = alteredChecks
        assert(configurationPresentation(unknownPermission)!.body.contains("Unknown: Reading, Withdrawals, Internal transfers, Universal transfers, IP restriction, Spot / margin trading"))

        var ethereum = binding
        ethereum["connection"] = ["provider": "ethereum", "name": "SYNTHETIC-wallet", "resource": "ethereum:1:0x0000000000000000000000000000000000000001", "credential_ref": NSNull(), "network": "ethereum:1", "address": "0x0000000000000000000000000000000000000001", "signing_enabled": false, "assets": [["network": "ethereum:1", "reference": "native", "decimals": 18]]] as [String: Any]
        ethereum["checks"] = [["name": "RPC chain", "status": "passed", "chain_id": 1], ["name": "wallet identity", "status": "passed", "signer_derived": false], ["name": "native balance read", "status": "passed", "atoms": "0"]]
        let wallet = configurationPresentation(ethereum)!.body
        assert(wallet.contains("Signing: disabled (read-only)"))
        assert(wallet.contains("0x0000000000000000000000000000000000000001"))
        assert(wallet.contains("Network: ethereum:1"))
        let proof = TerminalConfigurationReview(purpose: "configuration", bindingHash: "hash", nonce: "nonce", operation: "configuration:test", expiresAt: "expiry")
        func reviewed(_ candidate: [String: Any], _ hash: String = "hash", _ nonce: String = "nonce", _ operation: String = "configuration:test", _ expiry: String = "expiry", _ review: TerminalConfigurationReview? = proof) -> Bool {
            terminalConfigurationReviewed(review, binding: candidate, hash: hash, nonce: nonce, operation: operation, expiry: expiry, protectedChannels: true)
        }
        assert(reviewed(binding))
        assert(!terminalConfigurationReviewed(proof, binding: binding, hash: "hash", nonce: "nonce", operation: "configuration:test", expiry: "expiry", protectedChannels: false))
        assert(protectedReviewInput(mode: 0o100600, owner: 0, links: 0, flags: O_RDONLY))
        assert(!protectedReviewInput(mode: 0o100600, owner: 501, links: 0, flags: O_RDONLY))
        assert(!protectedReviewInput(mode: 0o100600, owner: 0, links: 1, flags: O_RDONLY))
        assert(!protectedReviewInput(mode: 0o010600, owner: 0, links: 0, flags: O_RDONLY))
        assert(!protectedReviewInput(mode: 0o100644, owner: 0, links: 0, flags: O_RDONLY))
        assert(!protectedReviewInput(mode: 0o100600, owner: 0, links: 0, flags: O_RDWR))
        assert(!protectedReviewInput(mode: 0o100600, owner: 0, links: 0, flags: -1))
        assert(protectedReviewOutput(mode: 0o010660, links: 0))
        assert(!protectedReviewOutput(mode: 0o010660, links: 1))
        assert(!protectedReviewOutput(mode: 0o100600, links: 0))
        assert(!reviewed(binding, "changed"))
        assert(!reviewed(binding, "hash", "changed"))
        assert(!reviewed(binding, "hash", "nonce", "financial:test"))
        assert(!reviewed(binding, "hash", "nonce", "configuration:test", "changed"))
        assert(!reviewed(binding, "hash", "nonce", "configuration:test", "expiry", nil))
        var financial = binding; financial["financial_movement"] = true
        assert(!reviewed(financial))
        financial = binding; financial["action"] = "transfer"
        assert(!reviewed(financial))
        financial = binding; financial["financial_movement"] = 0
        assert(!reviewed(financial))
        let setup: [String: Any] = ["action":"initialize", "financial_movement":false, "future_financial_authority":false]
        assert(configurationAuthenticationReason(setup) == "finish setup for your Mac account")
        let policy: [String: Any] = ["action":"policy_set", "financial_movement":false, "future_financial_authority":false]
        assert(configurationAuthenticationReason(policy) == "change your financial policy")
        assert(reviewed(policy))
        assert(!reviewed(policy, "changed"))
        for (action, verb) in [("add", "save"), ("update", "update"), ("remove", "remove")] {
            var candidate = binding; candidate["action"] = action
            assert(configurationAuthenticationReason(candidate) == "\(verb) the Binance connection \"SYNTHETIC-main\"")
        }
        assert(configurationAuthenticationReason(ethereum) == "save the Ethereum connection \"SYNTHETIC-wallet\"")
        var evm = ethereum
        var evmConnection = ethereum["connection"] as! [String: Any]
        evmConnection["provider"] = "evm"
        evm["connection"] = evmConnection
        assert(configurationAuthenticationReason(evm) == "save the EVM connection \"SYNTHETIC-wallet\"")
        let pricing: [String: Any] = ["action": "add", "financial_movement": false, "future_financial_authority": false,
                                      "connection": ["provider": "coingecko", "name": "SYNTHETIC-pricing"]]
        assert(configurationAuthenticationReason(pricing) == "save the CoinGecko connection \"SYNTHETIC-pricing\"")
        for field in ["financial_movement", "future_financial_authority"] {
            var candidate = setup; candidate[field] = true
            assert(configurationAuthenticationReason(candidate) == nil)
            candidate[field] = 0
            assert(configurationAuthenticationReason(candidate) == nil)
        }
        var badName = binding
        var badConnection = connection; badConnection["name"] = "SYNTHETIC\u{202e}hidden"
        badName["connection"] = badConnection
        assert(configurationAuthenticationReason(badName) == nil)
        assert(configurationAuthenticationReason([:]) == nil)
        let write: [String: Any] = ["version": 1, "operation": "synthetic.transfer",
                                   "execution": ["amount": "1", "destination": "SYNTHETIC-wallet"],
                                   "financial_intent": [:] as [String: Any]]
        let writeSummary = "Send 1 SYNTHETIC to SYNTHETIC-wallet; fee cap 0.01 SYNTHETIC."
        var deposit = write
        deposit["operation"] = "binance.deposit"
        let depositSummary = "Deposit 0.0000003 ETH: synthetic-wallet → synthetic-sub on Arbitrum One. Max fee 0.00001 ETH. USD ceilings: $0.01 movement, $0.01 fees."
        let depositReason = financialAuthenticationReason(deposit, summary: depositSummary)!
        assert(depositReason == "deposit 0.0000003 ETH: synthetic-wallet → synthetic-sub on Arbitrum One. Max fee 0.00001 ETH.")
        assert(!depositReason.contains("USD") && depositSummary.contains("USD ceilings:"))
        let depositWithoutCaps = "Deposit 1 USDC: wallet → savings on Arbitrum One. Max fee 0.001 ETH."
        assert(financialAuthenticationReason(deposit, summary: depositWithoutCaps) == "deposit 1 USDC: wallet → savings on Arbitrum One. Max fee 0.001 ETH.")
        // Unrecognized trailing text must not disappear along with USD limits.
        assert(financialAuthenticationReason(deposit, summary: depositWithoutCaps + " USD ceilings: invalid.")!.contains("invalid"))
        assert(financialAuthenticationReason(write, summary: depositSummary)!.contains("USD caps:"))
        assert(depositReason.count <= 180 && !depositReason.contains("full details"))
        assert(!depositReason.contains("native/18") && !depositReason.contains("42161") && !depositReason.contains("0x"))
        var bridge = write
        bridge["operation"] = "bridge.transfer"
        let bridgeSummary = "Bridge 10 USDC; dev-wallet Ethereum Mainnet → Arbitrum One via Relay; Receive ≥9.95 USDC; Max fees $0.50."
        let bridgeReason = financialAuthenticationReason(bridge, summary: bridgeSummary)!
        assert(bridgeReason == "bridge 10 USDC; dev-wallet Ethereum Mainnet → Arbitrum One via Relay; Receive ≥9.95 USDC; Max fees $0.50.")
        assert(bridgeReason.contains("Relay") && bridgeReason.contains("9.95 USDC") && bridgeReason.contains("$0.50"))
        let oversizedBridge = "Bridge 10 USDC; " + String(repeating: "long-alias", count: 30) + " Ethereum Mainnet → Arbitrum One via Relay; Receive ≥9.95 USDC; Max fees $0.50."
        let compactBridge = financialAuthenticationReason(bridge, summary: oversizedBridge)!
        assert(compactBridge.count <= 180 && compactBridge.contains("See terminal review for full details."))
        assert(compactBridge.contains("bridge 10 USDC;"))
        let oversizedDeposit = "Deposit 0.0000003 ETH: " + String(repeating: "long-alias", count: 30) + " on Arbitrum One. Max fee 0.00001 ETH."
        let oversizedReason = financialAuthenticationReason(deposit, summary: oversizedDeposit)!
        assert(oversizedReason.count <= 180 && oversizedReason.contains("See terminal review for full details."))
        assert(financialAuthenticationReason(write, summary: writeSummary) == "approve this action: \(writeSummary)")
        assert(financialAuthenticationReason(write, summary: "") == nil)
        assert(financialAuthenticationReason(write, summary: String(repeating: "x", count: 4097)) == nil)
        assert(financialAuthenticationReason(setup, summary: writeSummary) == nil)
        var incompleteWrite = write; incompleteWrite.removeValue(forKey: "execution")
        assert(financialAuthenticationReason(incompleteWrite, summary: writeSummary) == nil)
        incompleteWrite = write; incompleteWrite["version"] = 2
        assert(financialAuthenticationReason(incompleteWrite, summary: writeSummary) == nil)
        assert(financialAuthenticationReason(write, summary: "Sell 1\n\u{202e}hidden") == "approve this action: Sell 1\\u000a\\u202ehidden")
        let boundary: [String: Any] = ["version":1, "kind":"operation_boundaries", "operation":"binance.spot.order-place",
                                      "intent_id":"SYNTHETIC-intent", "fixed":["side":"SELL"],
                                      "parameters":["maximum_quantity":"2", "minimum_price":"10"],
                                      "maximum_debits":[["amount":"2"]], "maximum_costs":[[String:Any]](), "valid_for_seconds":600]
        let boundarySummary = "Sell up to 2 SYNTHETIC; minimum price 10 USD; repricing allowed; valid for 10 minutes."
        assert(boundaryAuthenticationReason(boundary, summary:boundarySummary) == "approve these boundaries: \(boundarySummary)")
        assert(financialAuthenticationReason(boundary, summary:boundarySummary) == nil)
        for operation in ["configuration:initialize", "policy.set", "connections.add"] {
            var administrative = boundary; administrative["operation"] = operation
            assert(boundaryAuthenticationReason(administrative, summary:boundarySummary) == nil)
        }
        for field in ["kind","fixed","parameters","maximum_debits","valid_for_seconds"] {
            var incomplete = boundary; incomplete.removeValue(forKey:field)
            assert(boundaryAuthenticationReason(incomplete, summary:boundarySummary) == nil)
        }
        var noLifetime = boundary; noLifetime["valid_for_seconds"] = true
        assert(boundaryAuthenticationReason(noLifetime, summary:boundarySummary) == nil)
        assert(boundaryAuthenticationReason(boundary, summary:"Sell\n\u{202e}hidden") == "approve these boundaries: Sell\\u000a\\u202ehidden")
        let address = "0x1111111111111111111111111111111111111111"
        let withdrawal = "Withdraw API quantity 0.00765 ETH from SYNTHETIC-main SPOT to SYNTHETIC-wallet (\(address)) on ARBITRUM; published fee 0.00005, approved maximum fee 0.00005, maximum gross debit 0.0077, minimum net receipt 0.0076. USD ceilings: $19.67 movement, $0.13 fees."
        let withdrawalReason = financialAuthenticationReason(write, summary: withdrawal)!
        assert(withdrawalReason.count <= authenticationReasonMaximumCharacters)
        assert(withdrawalReason.contains("0x1111…1111") && !withdrawalReason.contains(address))
        assert(withdrawalReason.contains("0.00765 ETH") && withdrawalReason.contains("ARBITRUM"))
        assert(withdrawalReason.contains("full details") || withdrawalReason.contains("min receipt 0.0076"))
        let shortAddress = financialAuthenticationReason(write, summary: "Send 1 ETH to \(address); fee cap 0.01 ETH.")!
        assert(shortAddress.contains("Send 1 ETH to 0x1111…1111; fee cap 0.01 ETH."))
        // Do not abbreviate an embedded identifier or an overlong hex string.
        for identifier in ["id\(address)", "\(address)a"] {
            assert(financialAuthenticationReason(write, summary: identifier)!.contains(identifier))
        }
        let longSummaries = [String(repeating: "x", count: 4096), String(repeating: "\u{202e}", count: 4096),
                             String(repeating: "🧑🏽‍💻", count: 1000), "Sell 2 SYNTHETIC; " + String(repeating: "limits ", count: 500)]
        for summary in longSummaries {
            for reason in [financialAuthenticationReason(write, summary: summary)!, boundaryAuthenticationReason(boundary, summary: summary)!] {
                assert(reason.count <= authenticationReasonMaximumCharacters)
                assert(reason.contains("See terminal review for full details."))
            }
        }
        let budget = authenticationReasonMaximumCharacters - "approve this action: ".count
        assert(financialAuthenticationReason(write, summary: String(repeating: "x", count: budget))!.count == authenticationReasonMaximumCharacters)
        assert(financialAuthenticationReason(write, summary: String(repeating: "x", count: budget + 1))!.contains("full details"))
        assert(write["execution"] as? [String: String] == ["amount": "1", "destination": "SYNTHETIC-wallet"])
        print("Approval presentation, terminal-proof/channel and authentication-reason cases passed; no UI or authentication requested.")
    }
}
