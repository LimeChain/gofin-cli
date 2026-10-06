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
        assert(childSummary.hasPrefix("Warning: Verify in Binance"))
        for text in ["UID 10001 belongs to synthetic@example.invalid", "SYNTHETIC-parent", "SYNTHETIC visible warning"] { assert(childSummary.contains(text)) }
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
        print("Approval presentation: 17 summary, 20 terminal-proof/channel and 13 authentication-reason cases passed; no UI or authentication requested.")
    }
}
