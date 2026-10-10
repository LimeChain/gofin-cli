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
        let bridgeSummary = "Bridge 10 USDC; dev-wallet Ethereum Mainnet → Arbitrum One via Relay; recipient dev-wallet; Receive ≥9.95 USDC; Max fees $0.50."
        let bridgeReason = financialAuthenticationReason(bridge, summary: bridgeSummary)!
        assert(bridgeReason == "bridge 10 USDC from Ethereum to Arbitrum. You’ll get at least 9.95 USDC. Total fees won’t exceed $0.50.")
        assert(!bridgeReason.contains("Relay") && !bridgeReason.contains("dev-wallet"))
        assert(bridgeReason.contains("9.95 USDC") && bridgeReason.contains("$0.50"))
        let otherRecipient = bridgeSummary.replacingOccurrences(of: "recipient dev-wallet", with: "recipient savings-wallet")
        assert(financialAuthenticationReason(bridge, summary: otherRecipient)!.contains("recipient savings-wallet"))
        let refundSummary = "Bridge 10 USDC; dev-wallet Ethereum Mainnet → Arbitrum One via Relay; recipient dev-wallet; Receive ≥9.95 USDC; Max fees $0.50; Total fees up to $0.50; Refund recipient 0x2222…2222; Refund paths Arbitrum One 0x2222…2222 ≥9.8 USDC."
        let refundReason = financialAuthenticationReason(bridge, summary: refundSummary)!
        assert(refundReason.contains("refund to 0x2222…2222") && refundReason.contains("refund path Arbitrum One 0x2222…2222 ≥9.8 USDC"))
        let oversizedBridge = "Bridge 10 USDC; " + String(repeating: "long-alias", count: 30) + " Ethereum Mainnet → Arbitrum One via Relay; Receive ≥9.95 USDC; Max fees $0.50."
        let compactBridge = financialAuthenticationReason(bridge, summary: oversizedBridge)!
        assert(compactBridge.count <= 180 && !compactBridge.contains("full details"))
        assert(compactBridge.contains("bridge 10 USDC from Ethereum to Arbitrum"))
        let oversizedDeposit = "Deposit 0.0000003 ETH: " + String(repeating: "long-alias", count: 30) + " on Arbitrum One. Max fee 0.00001 ETH."
        let oversizedReason = financialAuthenticationReason(deposit, summary: oversizedDeposit)!
        assert(oversizedReason.count <= 180 && oversizedReason.contains("See terminal review for full details."))
        assert(financialAuthenticationReason(write, summary: writeSummary) == "approve this action: \(writeSummary)")
        // The native reason receives the broker's reviewed synopsis. Keep the
        // swap's concrete route and all material ceilings visible when they fit;
        // the complete transaction binding is independently reviewed in Terminal.
        var swapBinding = write
        swapBinding["operation"] = "uniswap.swap"
        let swapSummary = "Swap 0.1 WETH for at least 200 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v3 fee 3000 ppm; router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline 1791500000; max network fee 0.001 ETH; permission: existing finite ERC-20 allowance to router 0x2222222222222222222222222222222222222222 was checked and reused."
        let swapReason = financialAuthenticationReason(swapBinding, summary: swapSummary)!
        for detail in ["0.1 WETH", "200 USDC", "recipient 0x1111…1111", "router 0x2222…2222",
                       "slippage 50 bps", "fee 3000 ppm", "max network fee 0.001 ETH", "deadline 1791500000"] {
            assert(swapReason.contains(detail), detail)
        }
        assert(swapReason.count <= ammAuthenticationReasonMaximumCharacters)
        var ramsesBinding = swapBinding
        ramsesBinding["operation"] = "ramses.swap"
        let ramsesSummary = "Swap 0.1 TOKEN for at least 190 USDG; recipient 0x1111111111111111111111111111111111111111; Ramses tick spacing 10, quoted pool fee 500 ppm (quote-time ceiling 1000 ppm only); router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline 1791500000; max network fee 0.001 ETH; permission: finite token allowance or typed EIP-2612 permit to Ramses router only after approval."
        let ramsesReason = financialAuthenticationReason(ramsesBinding, summary: ramsesSummary)!
        for detail in ["0.1 TOKEN", "190 USDG", "recipient 0x1111…1111", "Ramses tick spacing 10",
                       "quoted pool fee 500 ppm", "quote-time ceiling 1000 ppm only",
                       "router 0x2222…2222", "slippage 50 bps", "impact max 40 bps",
                       "deadline 1791500000", "max network fee 0.001 ETH", "only after approval"] {
            assert(ramsesReason.contains(detail), detail)
        }
        assert(ramsesReason.count <= ammAuthenticationReasonMaximumCharacters)
        let permitSummary = "Swap 0.01 WETH for at least 19 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v4 fee 500 ppm, tick spacing 10; router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline 1791500000; max network fee 0.001 ETH; permission: finite token allowance to canonical Permit2 was checked; a typed Permit2 signature will authorize the router only after approval."
        let permitReason = financialAuthenticationReason(swapBinding, summary: permitSummary)!
        for detail in ["0.01 WETH", "19 USDC", "recipient 0x1111…1111", "v4 fee 500 ppm", "router 0x2222…2222", "deadline 1791500000", "max network fee 0.001 ETH", "typed Permit2 signature", "after approval"] {
            assert(permitReason.contains(detail), detail)
        }
        let eip2612Summary = "Swap 0.1 WETH for at least 200 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v3 fee 3000 ppm; router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline 1791500000; max network fee 0.001 ETH; permission: typed EIP-2612 permit authorizes router 0x2222222222222222222222222222222222222222 only after approval."
        let eip2612Reason = financialAuthenticationReason(swapBinding, summary: eip2612Summary)!
        for detail in ["0.1 WETH", "200 USDC", "recipient 0x1111…1111", "EIP-2612", "router 0x2222…2222", "deadline 1791500000", "max network fee 0.001 ETH", "only after approval"] {
            assert(eip2612Reason.contains(detail), detail)
        }
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
        var bridgeBoundary = boundary
        bridgeBoundary["operation"] = "bridge.transfer"
        let walletAddress = "0x251500000000000000000000000000000000bd1c"
        bridgeBoundary["fixed"] = ["source_address": walletAddress, "refund_to": walletAddress]
        let bridgeGrantSummary = "Bridge 0.004 ETH; evm-dev Ethereum Mainnet → Arbitrum One via Relay; recipient evm-dev; Receive ≥0.0039 ETH; Cumulative max fees 0.0001 ETH gas, 0.00004 ETH withheld from output; Total fees up to 0.00014 ETH. Finite grant covers these declared steps: deposit source input 0.004 ETH."
        let bridgeGrantReason = boundaryAuthenticationReason(bridgeBoundary, summary: bridgeGrantSummary)!
        assert(bridgeGrantReason == "bridge 0.004 ETH from Ethereum to Arbitrum. You’ll get at least 0.0039 ETH. Total fees won’t exceed 0.00014 ETH.")
        assert(!bridgeGrantReason.contains("boundaries") && !bridgeGrantReason.contains("evm-dev") && !bridgeGrantReason.contains("Relay"))
        let sameWalletRefundSummary = "Bridge 0.004 ETH; evm-dev Ethereum Mainnet → Arbitrum One via Relay; recipient evm-dev; Receive ≥0.0039 ETH; Cumulative max fees 0.0001 ETH gas, 0.00004 ETH withheld from output; Total fees up to 0.00014 ETH; Refund recipient 0x2515…bd1c; Refund paths Ethereum Mainnet 0x2515…bd1c ≥0 ETH, Arbitrum One 0x2515…bd1c ≥0 ETH. Finite grant covers these declared steps: deposit source input 0.004 ETH."
        let sameWalletRefundReason = boundaryAuthenticationReason(bridgeBoundary, summary: sameWalletRefundSummary)!
        assert(sameWalletRefundReason.contains("refunds may be zero"))
        assert(!sameWalletRefundReason.contains("0x2515") && sameWalletRefundReason.contains("0.0039 ETH") && sameWalletRefundReason.contains("0.00014 ETH"))
        var differentRefundBoundary = bridgeBoundary
        differentRefundBoundary["fixed"] = ["source_address": walletAddress, "refund_to": "0x2222000000000000000000000000000000002222"]
        let differentRefundReason = boundaryAuthenticationReason(differentRefundBoundary, summary: sameWalletRefundSummary)!
        assert(differentRefundReason.contains("refund to 0x2515…bd1c") && differentRefundReason.contains("refund path Ethereum Mainnet 0x2515…bd1c ≥0 ETH, Arbitrum One 0x2515…bd1c ≥0 ETH"))
        let resetBridgeGrant = "Reset token allowance to 0 USDC for Relay spender 0xccc88a9d1b4ed6b0eaba998850414b24f1c315be; bridge 10 USDC; evm-dev Ethereum Mainnet → Arbitrum One via Relay; recipient evm-dev; Receive ≥9.95 USDC; Cumulative max fees 0.001 ETH gas; Total fees up to 0.001 ETH. Finite grant covers these declared steps: reset token allowance to 0 USDC; approve up to 12 USDC for Relay spender 0xccc88a9d1b4ed6b0eaba998850414b24f1c315be; deposit source input 10 USDC."
        let resetBridgeReason = boundaryAuthenticationReason(bridgeBoundary, summary: resetBridgeGrant)!
        assert(resetBridgeReason == "bridge 10 USDC from Ethereum to Arbitrum; includes allowance up to 12 USDC. You’ll get at least 9.95 USDC. Total fees won’t exceed 0.001 ETH.")

        let boundaryInputToken = "0x0000000000000000000000000000000000000064"
        let boundaryOutputToken = "0x0000000000000000000000000000000000000065"
        let boundarySigner = "0x1111111111111111111111111111111111111111"
        let boundaryRouter = "0x2222222222222222222222222222222222222222"
        let swapBoundary: [String: Any] = ["version":1, "kind":"operation_boundaries", "operation":"uniswap.swap",
                                          "intent_id":"SYNTHETIC-swap-authority",
                                          "fixed":["operation":"uniswap.swap", "network":"ethereum:1", "chain_id":1,
                                                   "wallet":"SYNTHETIC-wallet", "resource":"ethereum:1:\(boundarySigner)",
                                                   "signer":boundarySigner, "recipient":boundarySigner, "router":boundaryRouter,
                                                   "protocol":"v3", "token_in":["network":"ethereum:1", "reference":boundaryInputToken, "decimals":18],
                                                   "token_out":["network":"ethereum:1", "reference":boundaryOutputToken, "decimals":6], "v4_pool_key":NSNull()],
                                          "parameters":["max_input":"0.1", "max_slippage_bps":100,
                                                        "max_price_impact_bps":40, "minimum_rate":["input_atoms":"100000000000000000", "output_atoms":"190000000"],
                                                        "absolute_minimum_output":"190", "allowed_fee_tiers":[3000],
                                                        "max_fee_eth":"0.001", "max_deadline_seconds":600],
                                          "maximum_debits":[["amount":"0.1"]],
                                          "maximum_costs":[["amount":"0.001"]], "valid_for_seconds":600]
        let swapBoundarySummary = "Swap up to 0.1 \(boundaryInputToken) into \(boundaryOutputToken) via Uniswap v3; maximum native fee 0.001 ETH, slippage 100 bps, price impact 40 bps, deadline 600 seconds."
        let swapBoundaryReason = boundaryAuthenticationReason(swapBoundary, summary:swapBoundarySummary)!
        for detail in ["0x0000…0064", "0x0000…0065", "recipient 0x1111…1111", "router/spender 0x2222…2222",
                       "Max input 0.1", "max fee 0.001 ETH", "max slippage 100 bps", "impact 40 bps",
                       "190000000/100000000000000000 atoms", "absolute floor 190", "600s", "consent duration 600s",
                       "typed permit is finite"] {
            assert(swapBoundaryReason.contains(detail), detail)
        }
        assert(swapBoundaryReason.count <= ammAuthenticationReasonMaximumCharacters)
        var ramsesBoundary = swapBoundary
        ramsesBoundary["operation"] = "ramses.swap"
        var ramsesFixed = swapBoundary["fixed"] as! [String: Any]
        ramsesFixed["operation"] = "ramses.swap"; ramsesFixed["dex"] = "ramses"
        ramsesFixed["tick_spacing"] = 10
        ramsesFixed["factory"] = "0x4444444444444444444444444444444444444444"
        ramsesFixed["pool"] = "0x3333333333333333333333333333333333333333"
        ramsesFixed["pool_deployer"] = "0x5555555555555555555555555555555555555555"
        ramsesBoundary["fixed"] = ramsesFixed
        var ramsesBounds = swapBoundary["parameters"] as! [String: Any]
        ramsesBounds.removeValue(forKey: "allowed_fee_tiers")
        ramsesBounds["max_pool_fee_ppm"] = 1000
        ramsesBoundary["parameters"] = ramsesBounds
        let ramsesBoundaryReason = boundaryAuthenticationReason(ramsesBoundary, summary: "Ramses v3 bounded swap summary")!
        for detail in ["Ramses v3 tick spacing 10", "pool 0x3333…3333", "factory 0x4444…4444",
                       "pool deployer 0x5555…5555", "quote pool-fee ceiling ≤1000 ppm (quote-time bound only; no on-chain fee guarantee)",
                       "router/spender 0x2222…2222", "Max input 0.1", "minimum rate 190000000/100000000000000000 atoms",
                       "absolute floor 190", "deadline horizon 600s", "max fee 0.001 ETH"] {
            assert(ramsesBoundaryReason.contains(detail), detail)
        }
        assert(ramsesBoundaryReason.count <= ammAuthenticationReasonMaximumCharacters)
        var excessiveRamsesFee = ramsesBoundary
        var excessiveRamsesBounds = excessiveRamsesFee["parameters"] as! [String: Any]
        excessiveRamsesBounds["max_pool_fee_ppm"] = 1_000_000
        excessiveRamsesFee["parameters"] = excessiveRamsesBounds
        assert(boundaryAuthenticationReason(excessiveRamsesFee, summary: "Ramses bounded swap") == nil)
        for requiredIdentity in ["factory", "pool", "pool_deployer"] {
            var incompleteRamses = ramsesBoundary
            var incompleteFixed = incompleteRamses["fixed"] as! [String: Any]
            incompleteFixed.removeValue(forKey: requiredIdentity)
            incompleteRamses["fixed"] = incompleteFixed
            assert(boundaryAuthenticationReason(incompleteRamses, summary: "Ramses bounded swap") == nil)
        }
        for invalidSpacing in [0, 3, 25, 201] {
            var invalid = ramsesBoundary
            var invalidFixed = invalid["fixed"] as! [String: Any]
            invalidFixed["tick_spacing"] = invalidSpacing
            invalid["fixed"] = invalidFixed
            assert(boundaryAuthenticationReason(invalid, summary: "Ramses bounded swap") == nil)
        }
        var malformedSwapBoundary = swapBoundary
        var malformedSwapParameters = swapBoundary["parameters"] as! [String: Any]
        malformedSwapParameters.removeValue(forKey:"minimum_rate")
        malformedSwapBoundary["parameters"] = malformedSwapParameters
        assert(boundaryAuthenticationReason(malformedSwapBoundary, summary:swapBoundarySummary) == nil)
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
        let planStep: [String: Any] = ["step_id":"transfer", "operation":"wallet.transfer", "input":[String:Any](), "permitted_variations":["mode":"exact", "maximum_debits":[[String:Any]](), "maximum_costs":[[String:Any]](), "possible_credits":[[String:Any]]()], "completion":["operation_succeeded":true]]
        let planPermission: [String: Any] = ["step_id":"transfer", "exact":["scope":["mode":"exact", "maximum_debits":[[String:Any]](), "maximum_costs":[[String:Any]](), "possible_credits":[[String:Any]]()], "definition_hash":"schema", "connections_hash":"connections"]]
        let planBinding: [String: Any] = ["version":1, "kind":"financial_plan", "plan_id":"synthetic-plan", "revision_id":"synthetic-revision", "name":"Synthetic rotation", "plan":["name":"Synthetic rotation", "steps":[planStep]], "permissions":[planPermission], "valid_for_seconds":600]
        assert(financialPlanAuthenticationReason(planBinding, summary:"Full swap scope is in the terminal review.") == "approve the plan “Synthetic rotation” (1 steps, 600 seconds)")
        var generatedPermission = planPermission; generatedPermission["generated_input_fields"] = ["/requestId"]
        var generatedPlan = planBinding; generatedPlan["permissions"] = [generatedPermission]
        assert(financialPlanAuthenticationReason(generatedPlan, summary:"Full scope is in the terminal review.") == "approve the plan “Synthetic rotation” (1 steps, 600 seconds)")
        generatedPermission["generated_input_fields"] = "requestId"
        generatedPlan["permissions"] = [generatedPermission]
        assert(financialPlanAuthenticationReason(generatedPlan, summary:"review") == nil)
        var revisedPlan = planBinding; revisedPlan["replaces_revision_id"] = "synthetic-prior-revision"
        assert(financialPlanAuthenticationReason(revisedPlan, summary:"Full scope is in the terminal review.") == "approve the revised plan “Synthetic rotation” (1 steps, 600 seconds)")
        var incompletePlan = planBinding; incompletePlan["permissions"] = [[String:Any]]()
        assert(financialPlanAuthenticationReason(incompletePlan, summary:"review") == nil)
        incompletePlan = planBinding; incompletePlan["name"] = "spoofed"
        assert(financialPlanAuthenticationReason(incompletePlan, summary:"review") == nil)
        incompletePlan = planBinding; incompletePlan["name"] = "Bad\u{2028}name"; incompletePlan["plan"] = ["name":"Bad\u{2028}name", "steps":[planStep]]
        assert(financialPlanAuthenticationReason(incompletePlan, summary:"review") == nil)
        let longPlanName = String(repeating: "界", count: 80)
        var longPlan = planBinding; longPlan["name"] = longPlanName; longPlan["plan"] = ["name":longPlanName, "steps":[planStep]]
        let longPlanReason = financialPlanAuthenticationReason(longPlan, summary:"review")!
        assert(longPlanReason.hasPrefix("approve the plan “\(longPlanName)”") && longPlanReason.count <= authenticationReasonMaximumCharacters)
        let manyScalarsName = String(repeating: "🧑🏽‍💻", count: 20)
        var manyScalarsPlan = planBinding; manyScalarsPlan["name"] = manyScalarsName; manyScalarsPlan["plan"] = ["name":manyScalarsName, "steps":[planStep]]
        assert(financialPlanAuthenticationReason(manyScalarsPlan, summary:"review") == nil)

        let inputAddress = "0x0000000000000000000000000000000000000064"
        let outputAddress = "0x0000000000000000000000000000000000000065"
        let signerAddress = "0x1111111111111111111111111111111111111111"
        let routerAddress = "0x2222222222222222222222222222222222222222"
        let swapInput: [String: Any] = ["wallet":"SYNTHETIC-wallet", "network":"evm", "chain_id":1,
                                        "request_id":"SYNTHETIC-attempt", "token_in":inputAddress,
                                        "token_out":outputAddress, "amount":"0.1", "recipient":signerAddress,
                                        "min_output":"190", "slippage_bps":50, "max_price_impact_bps":40,
                                        "deadline":1791500000, "protocol":"v3", "fee_tier":3000, "max_fee_eth":"0.001"]
        let swapPlanStep: [String: Any] = ["step_id":"swap", "operation":"uniswap.swap", "input":swapInput,
                                           "permitted_variations":["max_input":"0.1", "max_fee_eth":"0.001",
                                                                    "max_slippage_bps":100, "max_price_impact_bps":40,
                                                                    "minimum_rate":["input_atoms":"100000000000000000", "output_atoms":"190000000"],
                                                                    "absolute_minimum_output":"190", "allowed_fee_tiers":[3000],
                                                                    "max_deadline_seconds":600],
                                           "completion":["operation_succeeded":true]]
        let inputAsset: [String: Any] = ["network":"ethereum:1", "reference":inputAddress, "decimals":18]
        let outputAsset: [String: Any] = ["network":"ethereum:1", "reference":outputAddress, "decimals":6]
        let nativeAsset: [String: Any] = ["network":"ethereum:1", "reference":"native", "decimals":18]
        let ammSpec: [String: Any] = ["fixed":["operation":"uniswap.swap", "network":"ethereum:1", "chain_id":1,
                                     "wallet":"SYNTHETIC-wallet", "resource":"ethereum:1:\(signerAddress)",
                                     "signer":signerAddress, "recipient":signerAddress, "router":routerAddress,
                                     "protocol":"v3", "token_in":inputAsset, "token_out":outputAsset,
                                     "v4_pool_key":NSNull()],
                                      "parameters":["max_input":"0.1", "max_fee_eth":"0.001",
                                                    "max_slippage_bps":100, "max_price_impact_bps":40,
                                                    "minimum_rate":["input_atoms":"100000000000000000", "output_atoms":"190000000"],
                                                    "absolute_minimum_output":"190", "allowed_fee_tiers":[3000],
                                                    "max_deadline_seconds":600],
                                      "maximum_debits":[["resource":"ethereum:1:\(signerAddress)",
                                                          "amount":["asset":inputAsset, "atoms":"100000000000000000"]]],
                                      "maximum_costs":[["location":["kind":"cash", "resource":"ethereum:1:\(signerAddress)"],
                                                         "amount":["asset":nativeAsset, "atoms":"1000000000000000"],
                                                         "treatment":"additional_debit"]],
                                      "summary":"Swap up to 0.1 \(inputAddress) into \(outputAddress) via Uniswap v3; maximum native fee 0.001 ETH, slippage 100 bps, price impact 40 bps, deadline 600 seconds."]
        let ammPlanPermission: [String: Any] = ["step_id":"swap", "permission":["spec":ammSpec,
                                                                        "possible_credits":[["resource":"ethereum:1:\(signerAddress)", "asset":outputAsset]],
                                                                        "fee_assets":[nativeAsset]]]
        let ammPlan: [String: Any] = ["version":1, "kind":"financial_plan", "plan_id":"synthetic-amm-plan",
                                      "revision_id":"synthetic-amm-revision", "name":"Synthetic bounded swap",
                                      "plan":["name":"Synthetic bounded swap", "steps":[swapPlanStep],
                                              "aggregate_bounds":["maximum_debits":[["resource":"ethereum:1:\(signerAddress)",
                                                                                      "amount":["asset":inputAsset, "atoms":"100000000000000000"]]],
                                                                  "maximum_costs":[["location":["kind":"cash", "resource":"ethereum:1:\(signerAddress)"],
                                                                                     "amount":["asset":nativeAsset, "atoms":"1000000000000000"],
                                                                                     "treatment":"additional_debit"]]]],
                                      "permissions":[ammPlanPermission], "valid_for_seconds":600]
        let ammPlanReason = financialPlanAuthenticationReason(ammPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.")!
        for detail in ["Synthetic bounded swap", "0.1 \(inputAddress.prefix(6))…\(inputAddress.suffix(4))",
                       "min 190 \(outputAddress.prefix(6))…\(outputAddress.suffix(4))", "recipient \(signerAddress.prefix(6))…\(signerAddress.suffix(4))",
                       "router/spender \(routerAddress.prefix(6))…\(routerAddress.suffix(4))", "fee tier 3000", "slip 50/100 bps",
                       "min rate 190000000/100000000000000000 atoms", "gas ≤0.001 ETH", "1791500000", "100000000000000000 atoms",
                       "1000000000000000 atoms"] {
            assert(ammPlanReason.contains(detail), detail)
        }
        assert(ammPlanReason.count <= ammAuthenticationReasonMaximumCharacters)
        var ramsesInput = swapInput
        ramsesInput.removeValue(forKey: "fee_tier")
        ramsesInput["tick_spacing"] = 10
        ramsesInput["max_pool_fee_ppm"] = 1000
        var ramsesPlanStep = swapPlanStep
        ramsesPlanStep["operation"] = "ramses.swap"
        ramsesPlanStep["input"] = ramsesInput
        var ramsesPlanFixed = ammSpec["fixed"] as! [String: Any]
        ramsesPlanFixed["operation"] = "ramses.swap"
        ramsesPlanFixed["dex"] = "ramses"
        ramsesPlanFixed["tick_spacing"] = 10
        ramsesPlanFixed["factory"] = "0x4444444444444444444444444444444444444444"
        ramsesPlanFixed["pool"] = "0x3333333333333333333333333333333333333333"
        ramsesPlanFixed["pool_deployer"] = "0x5555555555555555555555555555555555555555"
        var ramsesPlanParameters = ammSpec["parameters"] as! [String: Any]
        ramsesPlanParameters.removeValue(forKey: "allowed_fee_tiers")
        ramsesPlanParameters["max_pool_fee_ppm"] = 1000
        var ramsesSpec = ammSpec
        ramsesSpec["fixed"] = ramsesPlanFixed
        ramsesSpec["parameters"] = ramsesPlanParameters
        ramsesSpec["summary"] = "Ramses v3 tick spacing 10; quote fee ceiling 1000 ppm; finite swap boundary."
        var ramsesPlanPermission = ammPlanPermission
        ramsesPlanPermission["permission"] = ["spec":ramsesSpec,
                                               "possible_credits":[["resource":"ethereum:1:\(signerAddress)", "asset":outputAsset]],
                                               "fee_assets":[nativeAsset]] as [String: Any]
        var ramsesPlan = ammPlan
        var ramsesPlanBody = ammPlan["plan"] as! [String: Any]
        ramsesPlanBody["steps"] = [ramsesPlanStep]
        ramsesPlan["plan"] = ramsesPlanBody
        ramsesPlan["permissions"] = [ramsesPlanPermission]
        let ramsesPlanReason = financialPlanAuthenticationReason(ramsesPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.")!
        for detail in ["Ramses v3 tick spacing 10", "pool 0x3333…3333", "factory 0x4444…4444",
                       "pool deployer 0x5555…5555", "quote pool-fee ceiling ≤1000 ppm (quote-time bound only; no on-chain fee guarantee)",
                       "min 190", "router/spender", "slip 50/100 bps", "gas ≤0.001 ETH", "deadline 1791500000"] {
            assert(ramsesPlanReason.contains(detail), detail)
        }
        assert(ramsesPlanReason.count <= ammAuthenticationReasonMaximumCharacters)
        var overWideRamsesPlan = ramsesPlan
        var overWideRamsesSteps = ramsesPlanBody
        var overWideRamsesStep = ramsesPlanStep
        var overWideInput = ramsesInput
        overWideInput["max_pool_fee_ppm"] = 1001
        overWideRamsesStep["input"] = overWideInput
        overWideRamsesSteps["steps"] = [overWideRamsesStep]
        overWideRamsesPlan["plan"] = overWideRamsesSteps
        assert(financialPlanAuthenticationReason(overWideRamsesPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.") == nil)
        assert(!ammPlanReason.contains("See terminal review for full details."))
        var twoSwapPlan = ammPlan
        var twoSwapStep = swapPlanStep; twoSwapStep["step_id"] = "swap-2"
        var twoSwapPermission = ammPlanPermission; twoSwapPermission["step_id"] = "swap-2"
        var twoStepBody = ammPlan["plan"] as! [String: Any]; twoStepBody["steps"] = [swapPlanStep, twoSwapStep]
        twoSwapPlan["plan"] = twoStepBody; twoSwapPlan["permissions"] = [ammPlanPermission, twoSwapPermission]
        let twoSwapReason = financialPlanAuthenticationReason(twoSwapPlan, summary:"Approve the plan “Synthetic bounded swap” with 2 ordered steps for 600 seconds.")!
        assert(twoSwapReason.count <= ammAuthenticationReasonMaximumCharacters)
        assert(twoSwapReason.contains("Swap step swap:" ) && twoSwapReason.contains("Swap step swap-2:"))
        assert(!twoSwapReason.contains("See terminal review for full details."))
        var malformedAMMPlan = ammPlan
        var malformedAMMSpec = ammSpec
        malformedAMMSpec.removeValue(forKey:"parameters")
        var malformedPermission = ammPlanPermission
        malformedPermission["permission"] = ["spec":malformedAMMSpec, "possible_credits":[["resource":"ethereum:1:\(signerAddress)", "asset":outputAsset]], "fee_assets":[nativeAsset]] as [String: Any]
        malformedAMMPlan["permissions"] = [malformedPermission]
        assert(financialPlanAuthenticationReason(malformedAMMPlan, summary:"review") == nil)
        var longAMMPlan = ammPlan
        var longSteps: [[String: Any]] = []
        var longPermissions: [[String: Any]] = []
        for index in 1...3 {
            var step = swapPlanStep; step["step_id"] = "swap-\(index)"; longSteps.append(step)
            var permission = ammPlanPermission; permission["step_id"] = "swap-\(index)"; longPermissions.append(permission)
        }
        var longPlanBody = ammPlan["plan"] as! [String: Any]; longPlanBody["steps"] = longSteps
        longAMMPlan["plan"] = longPlanBody; longAMMPlan["permissions"] = longPermissions
        let longAMMReason = financialPlanAuthenticationReason(longAMMPlan, summary:"Approve the plan “Synthetic bounded swap” with 3 ordered steps for 600 seconds.")!
        assert(longAMMReason.count <= ammAuthenticationReasonMaximumCharacters)
        assert(longAMMReason.contains("See terminal review for full details."))
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
