import Foundation

// Synthetic malformed consent fixtures are shared by boundary and plan tests.
private func invalidPoolMapScopes(fixed: [String: Any], parameters: [String: Any], tier: Int, dex: String)
    -> [(String, [String: Any], [String: Any])] {
    let pool = "0x3333333333333333333333333333333333333333"
    let zero = "0x" + String(repeating: "0", count: 40)
    var cases: [(String, [String: Any], [String: Any])] = []
    for (label, value) in [
        ("null map", NSNull() as Any), ("array map", [pool] as Any),
        ("empty map", [String: Any]() as Any),
        ("noncanonical tier", ["0\(tier)":pool] as Any),
        ("unknown tier", ["1234":pool] as Any),
        ("wrong venue tier", [String(dex == "sushi" ? 2500 : 3000):pool] as Any),
        ("zero pool", [String(tier):zero] as Any),
        ("malformed pool", [String(tier):"SYNTHETIC-not-an-address"] as Any),
        ("null pool", [String(tier):NSNull()] as Any),
        ("nonstring pool", [String(tier):123] as Any),
        ("extra entry", [String(tier):pool, "500":"0x6666666666666666666666666666666666666666"] as Any)
    ] {
        var invalid = fixed; invalid["approved_pools"] = value
        cases.append((label, invalid, parameters))
    }
    for (label, value) in [("both pool and map", pool as Any), ("null pool with map", NSNull() as Any)] {
        var invalid = fixed; invalid["pool"] = value
        cases.append((label, invalid, parameters))
    }
    for (label, value) in [
        ("no allowed tiers", [] as Any), ("duplicate tiers", [tier, tier] as Any),
        ("missing map entry", [tier, 500] as Any), ("boolean tier", [true] as Any),
        ("too many tiers", [100, 500, tier, 10000, tier] as Any)
    ] {
        var invalid = parameters; invalid["allowed_fee_tiers"] = value
        cases.append((label, fixed, invalid))
    }
    var crossDex = fixed; crossDex["dex"] = dex == "sushi" ? "pancakeswap" : "sushi"
    cases.append(("DEX cross-scope", crossDex, parameters))
    var wrongDeployer = fixed; wrongDeployer["pool_deployer"] = dex == "sushi" ? pool as Any : zero as Any
    cases.append(("deployer cross-scope", wrongDeployer, parameters))
    var noDeployer = fixed; noDeployer.removeValue(forKey:"pool_deployer")
    if dex == "pancakeswap" { cases.append(("missing deployer", noDeployer, parameters)) }
    return cases
}

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
        assert(main.title == "Save GoFin connection")
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
        assert(nativeApprovalReasonForOS(configurationAuthenticationReason(policy)!) == "change your financial policy")
        assert(reviewed(policy))
        let administrative: [String: Any] = ["action":"configure", "financial_movement":false, "future_financial_authority":false,
            "operation":"krakenfutures.set-self-trade-strategy", "product":"krakenfutures", "account":"SYNTHETIC-futures", "parameters":["strategy":"REJECT_TAKER"]]
        assert(reviewed(administrative))
        assert(configurationAuthenticationReason(administrative) == "configure krakenfutures.set-self-trade-strategy for account SYNTHETIC-futures")
        var crossedProduct = administrative; crossedProduct["product"] = "kraken"
        assert(configurationAuthenticationReason(crossedProduct) == nil)
        crossedProduct["operation"] = "kraken.remove-export"; crossedProduct["product"] = "krakenfutures"
        assert(configurationAuthenticationReason(crossedProduct) == nil)
        var administrativeFunds = administrative; administrativeFunds["financial_movement"] = true
        assert(!reviewed(administrativeFunds)); assert(configurationAuthenticationReason(administrativeFunds) == nil)
        var malformedAdmin = administrative; malformedAdmin.removeValue(forKey:"parameters")
        assert(configurationAuthenticationReason(malformedAdmin) == nil)
        for (provider, title) in [("kraken", "Kraken"), ("krakenfutures", "Kraken Futures")] {
            var venue = binding; var account = connection; account["provider"] = provider; venue["connection"] = account
            assert(configurationAuthenticationReason(venue) == "save the \(title) connection \"SYNTHETIC-main\"")
        }

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
        let solanaConnection: [String: Any] = [
            "provider": "solana", "name": "SYNTHETIC-solana", "resource": "solana:mainnet-beta:11111111111111111111111111111111",
            "credential_ref": "SYNTHETIC-SOLANA-REFERENCE", "network": "solana:mainnet-beta",
            "address": "11111111111111111111111111111111", "signing_enabled": true,
        ]
        var solanaReadOnly = solanaConnection
        solanaReadOnly["name"] = "SYNTHETIC-solana-watch"
        solanaReadOnly["credential_ref"] = NSNull()
        solanaReadOnly["signing_enabled"] = false
        let jupiterConnection: [String: Any] = [
            "provider": "jupiter", "name": "SYNTHETIC-jupiter", "resource": "jupiter:service",
            "credential_ref": "SYNTHETIC-JUPITER-REFERENCE",
        ]
        for (connection, title) in [(solanaConnection, "Solana"), (solanaReadOnly, "Solana"), (jupiterConnection, "Jupiter")] {
            var candidate = binding
            candidate["connection"] = connection
            candidate["checks"] = title == "Solana" ? [
                ["name": "mainnet-beta RPC identity", "status": "passed"],
                ["name": "wallet identity", "status": "passed", "signer_derived": connection["signing_enabled"]!],
                ["name": "native balance read", "status": "passed", "lamports": 0],
            ] : [["name": "protected service credential", "status": "passed"]]
            for (action, verb) in [("add", "save"), ("update", "update"), ("remove", "remove")] {
                candidate["action"] = action
                assert(configurationAuthenticationReason(candidate) == "\(verb) the \(title) connection \"\(connection["name"] as! String)\"")
                assert(reviewed(candidate))
                assert(!reviewed(candidate, "changed"))
                var movement = candidate; movement["financial_movement"] = true
                assert(!reviewed(movement) && configurationAuthenticationReason(movement) == nil)
                var authority = candidate; authority["future_financial_authority"] = true
                assert(!reviewed(authority) && configurationAuthenticationReason(authority) == nil)
            }
        }
        let pricing: [String: Any] = ["action": "add", "financial_movement": false, "future_financial_authority": false,
                                      "connection": ["provider": "coingecko", "name": "SYNTHETIC-pricing"]]
        assert(configurationAuthenticationReason(pricing) == "save the CoinGecko connection \"SYNTHETIC-pricing\"")
        for (provider, title) in [
            ("coinbase", "Coinbase"),
            ("coinbaseexchange", "Coinbase Exchange"),
            ("coinbaseinternational", "Coinbase International"),
        ] {
            let identity = "SYNTHETIC-portfolio-123"
            let coinbaseConnection: [String: Any] = [
                "provider": provider,
                "name": "SYNTHETIC-coinbase",
                "resource": "\(provider):portfolio:\(identity)",
                "credential_ref": "11111111-1111-4111-8111-111111111111",
                "physical_identity": identity,
                "physical_identity_verified": true,
                "portfolio_id": identity,
            ]
            var coinbase = binding
            coinbase["connection"] = coinbaseConnection
            let permissionEvidence: [String: Any] = [
                "can_view": true, "can_trade": false, "can_transfer": false, "can_receive": true,
            ]
            coinbase["checks"] = [
                ["name": "authenticated credential read", "status": "passed"],
                ["name": "portfolio identity", "status": "venue_verified", "portfolio_id": identity],
            ]
            if provider == "coinbase" {
                coinbase["checks"] = (coinbase["checks"] as! [[String: Any]]) + [
                    ["name": "reported API permissions", "status": "observed", "permissions": permissionEvidence],
                    ["name": "permissioned portfolio binding", "status": "passed", "permissions": permissionEvidence],
                ]
            }
            for (action, verb) in [("add", "save"), ("update", "update"), ("remove", "remove")] {
                coinbase["action"] = action
                if action == "remove" {
                    coinbase.removeValue(forKey: "checks")
                    coinbase.removeValue(forKey: "warnings")
                    coinbase["warning"] = "Keep independent recovery credentials for any remaining external funds or positions."
                }
                let reason = "\(verb) the \(title) connection \"SYNTHETIC-coinbase\""
                assert(configurationAuthenticationReason(coinbase) == reason)
                assert(reviewed(coinbase))
                assert(!reason.contains("11111111-1111-4111-8111-111111111111"))
                assert(!terminalConfigurationReviewed(proof, binding: coinbase, hash: "wrong-hash", nonce: "nonce", operation: "configuration:test", expiry: "expiry", protectedChannels: true))
                var movement = coinbase
                movement["financial_movement"] = true
                assert(!reviewed(movement) && configurationAuthenticationReason(movement) == nil)
                var authority = coinbase
                authority["future_financial_authority"] = true
                assert(!reviewed(authority) && configurationAuthenticationReason(authority) == nil)
            }
            var unknownProvider = coinbase
            var unknownConnection = coinbaseConnection
            unknownConnection["provider"] = "coinbaseunknown"
            unknownProvider["connection"] = unknownConnection
            assert(configurationAuthenticationReason(unknownProvider) == nil)
            var crossedProduct = coinbase
            var crossedConnection = coinbaseConnection
            crossedConnection["resource"] = "\(provider == "coinbase" ? "coinbaseexchange" : "coinbase"):portfolio:\(identity)"
            crossedProduct["connection"] = crossedConnection
            assert(configurationAuthenticationReason(crossedProduct) == nil)
            var unverifiedIdentity = coinbase
            var unverifiedConnection = coinbaseConnection
            unverifiedConnection["physical_identity_verified"] = 1
            unverifiedIdentity["connection"] = unverifiedConnection
            assert(configurationAuthenticationReason(unverifiedIdentity) == nil)
            var mismatchedPortfolio = coinbase
            var mismatchedConnection = coinbaseConnection
            mismatchedConnection["portfolio_id"] = "SYNTHETIC-other-portfolio"
            mismatchedPortfolio["connection"] = mismatchedConnection
            assert(configurationAuthenticationReason(mismatchedPortfolio) == nil)
            var legacyPortfolio = coinbase
            var legacyConnection = coinbaseConnection
            legacyConnection.removeValue(forKey: "portfolio_id")
            legacyPortfolio["connection"] = legacyConnection
            legacyPortfolio["action"] = "add"
            assert(configurationAuthenticationReason(legacyPortfolio) == "save the \(title) connection \"SYNTHETIC-coinbase\"")
        }
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
        let coinbaseExecution: [String: Any] = [
            "feeMode":"venue_calculated", "network":"base", "currency":"USDC",
            "amount":"1", "feeReserveUsdc":"0.1", "destinationName":"SYNTHETIC-wallet",
            "destinationAddress":"0x1111111111111111111111111111111111111111",
            "resource":"coinbase:portfolio:SYNTHETIC",
        ]
        let coinbaseCosts: [[String: Any]] = [[
            "treatment":"venue_calculated_additional_debit",
            "location":["kind":"cash", "resource":"coinbase:portfolio:SYNTHETIC"],
            "amount":["asset":["network":"coinbase", "reference":"USDC", "decimals":36], "atoms":"100000000000000000000000000000000000"],
        ]]
        var coinbaseWithdrawal: [String: Any] = [
            "version":1, "operation":"coinbase.withdraw", "execution":coinbaseExecution,
            "financial_intent":["maximum_costs":coinbaseCosts],
        ]
        let coinbaseReason = financialAuthenticationReason(coinbaseWithdrawal, summary:"Misleading fee-free summary")!
        assert(coinbaseReason.contains("Coinbase-calculated additional fees (not capped)"))
        assert(coinbaseReason.contains("withdraw 1 USDC") && coinbaseReason.contains("Base"))
        assert(coinbaseReason.contains("0.1 USDC") && !coinbaseReason.contains("fee-free"))
        assert(coinbaseReason.count <= authenticationReasonMaximumCharacters)
        for (key, invalid) in [("feeMode","capped"), ("network","ethereum"), ("currency","ETH"),
                               ("amount","0"), ("feeReserveUsdc","0"), ("feeReserveUsdc","0.2"),
                               ("destinationAddress","bad-address")] {
            var execution = coinbaseExecution; execution[key] = invalid
            var candidate = coinbaseWithdrawal; candidate["execution"] = execution
            assert(financialAuthenticationReason(candidate, summary:"withdraw") == nil)
        }
        var missingConsent = coinbaseExecution; missingConsent.removeValue(forKey:"feeMode")
        coinbaseWithdrawal["execution"] = missingConsent
        assert(financialAuthenticationReason(coinbaseWithdrawal, summary:"withdraw") == nil)
        var longCoinbaseExecution = coinbaseExecution
        longCoinbaseExecution["destinationName"] = String(repeating:"x", count:128)
        longCoinbaseExecution["amount"] = String(repeating:"1", count:70)
        coinbaseWithdrawal["execution"] = longCoinbaseExecution
        let longCoinbaseReason = financialAuthenticationReason(coinbaseWithdrawal, summary:"withdraw")!
        assert(longCoinbaseReason.contains("not capped") && longCoinbaseReason.count <= authenticationReasonMaximumCharacters)
        coinbaseWithdrawal["execution"] = coinbaseExecution
        for (atoms, decimals) in [("0",36), ("1",36), ("100000",6)] {
            var costs = coinbaseCosts
            costs[0]["amount"] = ["asset":["network":"coinbase", "reference":"USDC", "decimals":decimals], "atoms":atoms]
            coinbaseWithdrawal["financial_intent"] = ["maximum_costs":costs]
            assert(financialAuthenticationReason(coinbaseWithdrawal, summary:"withdraw") == nil)
        }
        coinbaseWithdrawal["financial_intent"] = ["maximum_costs": [["treatment":"additional_debit"]]]
        assert(financialAuthenticationReason(coinbaseWithdrawal, summary:"withdraw") == nil)
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
        assert(approvalDuration(600) == "10 minutes")
        assert(approvalDuration(60) == "1 minute")
        assert(approvalDuration(90) == "1 minute 30 seconds")
        assert(reviewedNetworkName("ethereum:4663") == "Robinhood Chain")
        let futureDeadline = UInt64(Date().timeIntervalSince1970) + 600
        // The OS prompt gets the concrete human action, while exact route data
        // and formulas remain available in the structured terminal review.
        var swapBinding = write
        swapBinding["operation"] = "uniswap.swap"
        let promptInputAsset: [String: Any] = ["network":"ethereum:1", "reference":"0x1111111111111111111111111111111111111111", "decimals":18]
        let promptOutputAsset: [String: Any] = ["network":"ethereum:1", "reference":"0x2222222222222222222222222222222222222222", "decimals":6]
        let promptNativeAsset: [String: Any] = ["network":"ethereum:1", "reference":"native", "decimals":18]
        swapBinding["execution"] = ["token":promptInputAsset,
                                     "units":"100000000000000000",
                                     "output":promptOutputAsset,
                                     "minimum_output":"200000000",
                                     "deadline":futureDeadline, "permit_template":NSNull()] as [String: Any]
        swapBinding["financial_intent"] = ["maximum_costs":[["amount":["asset":promptNativeAsset, "atoms":"1000000000000000"]]]] as [String: Any]
        let swapSummary = "Swap 0.1 WETH for at least 200 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v3 fee 3000 ppm; router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline \(futureDeadline); max network fee 0.001 ETH; permission: existing finite ERC-20 allowance was checked and reused."
        let swapReason = financialAuthenticationReason(swapBinding, summary: swapSummary)!
        for detail in ["allow swapping 0.1 WETH for at least 200 USDC", "max network fees 0.001 ETH", "expires in "] {
            assert(swapReason.contains(detail), detail)
        }
        for hidden in ["0x", "router", "ppm", "bps", "atoms", String(futureDeadline)] { assert(!swapReason.contains(hidden), hidden) }
        assert(swapReason.count <= 300)
        assert(nativeApprovalReasonForOS(swapReason + ". \n") == String(swapReason.dropLast()))
        assert(nativeApprovalReasonForOS("allow swap up to 0.001 ETH.") == "allow swap up to 0.001 ETH")
        assert(!nativeApprovalReasonForOS(swapReason + ".").contains("…"))

        let bpMint = "BPxxfRCXkUVhig4HS1Lh7kZqV6SPJhzfEk4x6fVBjPCy"
        let bonkMint = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"
        let solWallet = "11111111111111111111111111111111"
        let solResource = "solana:mainnet-beta:\(solWallet)"
        let bpAsset: [String: Any] = ["network":"solana:mainnet-beta", "reference":bpMint, "decimals":9]
        let bonkAsset: [String: Any] = ["network":"solana:mainnet-beta", "reference":bonkMint, "decimals":5]
        let solAsset: [String: Any] = ["network":"solana:mainnet-beta", "reference":"native", "decimals":9]
        let solConversion: [String: Any] = ["kind":"conversion", "resource":solResource,
            "input":bpAsset, "maximum_input":"123456789123", "output":bonkAsset, "minimum_output":"9876543210000"]
        let solFundingEffect: [String: Any] = ["kind":"debit", "resource":solResource,
            "asset":solAsset, "maximum_debit":"1488440"]
        let jupiterBinding: [String: Any] = ["version":1, "operation":"jupiter.swap",
            "execution":["wallet_address":solWallet, "input_atoms":"123456789123",
                         "minimum_output_atoms":"9876543210000", "maximum_network_fee_lamports":"1000000",
                         "account_funding_lamports":1488440, "funding_accounts":[["payer":solWallet,"lamports":1488440]],
                         "platform_fee_bps":10] as [String: Any],
            "financial_intent":["expected":[solConversion, solFundingEffect],
                "debits":[["resource":solResource, "amount":["asset":bpAsset, "atoms":"123456789123"]],
                          ["resource":solResource, "amount":["asset":solAsset, "atoms":"1488440"]]],
                "maximum_costs":[["location":["kind":"cash", "resource":solResource],
                    "amount":["asset":solAsset, "atoms":"1000000"], "treatment":"additional_debit"]]] as [String: Any]]
        let jupiterRawSummary = "Swap 123456789123 atoms of \(bpMint) for at least 9876543210000 atoms of \(bonkMint) on Solana mainnet-beta."
        let jupiterReason = financialAuthenticationReason(jupiterBinding, summary:jupiterRawSummary)!
        assert(jupiterReason.contains("123.456789123 BP for at least 98765432.1 BONK on Solana"))
        assert(jupiterReason.contains("max network fees 0.001 SOL; platform fee 0.1%; account funding up to 0.00148844 SOL"))
        assert(!jupiterReason.contains("atoms") && !jupiterReason.contains(bpMint) && !jupiterReason.contains(bonkMint))

        let meteoraWallet = "11111111111111111111111111111111"
        let meteoraPool = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6"
        let wsolMint = "So11111111111111111111111111111111111111112"
        let usdcMint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        let meteoraSummary = "Swap 0.01 SOL for at least 1.059423 USDC on Meteora DLMM. Pool fee (included): 0.000004 SOL. Network fee: 0.00001 SOL (max 0.00002). Account funding: 0.00148844 SOL (max 0.002). SOL: 7.476550091 available; 0.01149844 required; 7.465051651 remaining."
        func meteoraSwapBinding(family: String, inputMint: String = "native") -> [String: Any] {
            let inputDecimals = inputMint == "native" || inputMint == wsolMint ? 9 : 6
            let inputAsset: [String: Any] = ["network":"solana:mainnet-beta", "reference":inputMint, "decimals":inputDecimals]
            let outputAsset: [String: Any] = ["network":"solana:mainnet-beta", "reference":usdcMint, "decimals":6]
            let inputAtoms = inputMint == "native" || inputMint == wsolMint ? "10000000" : "10000"
            let minimumAtoms = inputMint == "native" || inputMint == wsolMint ? "1059423" : "1059"
            let pool: [String: Any] = [
                "family":family, "address":meteoraPool,
                "token_a":["mint":wsolMint, "decimals":9, "token_program":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "transfer_fee":NSNull()] as [String: Any],
                "token_b":["mint":usdcMint, "decimals":6, "token_program":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "transfer_fee":NSNull()] as [String: Any],
                "active":true, "current_bin":0, "fee_bps":NSNull(), "protocol_state":NSNull(),
            ] as [String: Any]
            let quote: [String: Any] = [
                "family":family, "action":"swap", "pool":meteoraPool,
                "position":NSNull(), "token_a_debit_atoms":inputMint == "native" || inputMint == wsolMint ? inputAtoms : "0",
                "token_b_debit_atoms":inputMint == usdcMint ? inputAtoms : "0",
                "token_a_credit_atoms":"0", "token_b_credit_atoms":minimumAtoms,
                "receipt_delta_atoms":NSNull(), "fees_a_atoms":"4000", "fees_b_atoms":"0",
                "rewards":[], "expires_at":"2026-10-16T07:00:00Z", "price_impact_bps":0,
                "native_cash_delta":inputMint == "native" ? "-10000000" as Any : NSNull(), "protocol_details":[:],
            ] as [String: Any]
            let expected: [String: Any] = [
                "kind":"conversion", "resource":"solana:mainnet-beta:\(meteoraWallet)",
                "input":inputAsset, "maximum_input":inputAtoms,
                "output":outputAsset, "minimum_output":minimumAtoms,
            ]
            let nativeFeeAsset: [String: Any] = ["network":"solana:mainnet-beta", "reference":"native", "decimals":9]
            let feeCost: [String: Any] = [
                "location":["kind":"cash", "resource":"solana:mainnet-beta:\(meteoraWallet)"],
                "amount":["asset":nativeFeeAsset, "atoms":"20000"], "treatment":"additional_debit",
            ]
            return [
                "version":1, "operation":"meteora.swap",
                "execution":[
                    "wallet_address":meteoraWallet, "pool":pool,
                    "protocol_candidate":["quote":quote, "instructions":[], "lookup_tables":[], "blockhash":"synthetic", "last_valid_block_height":900, "stable_position_address":NSNull(), "required_secondary_signers":[], "metadata":[:]],
                    "maximum_network_fee_lamports":20000, "maximum_account_funding_lamports":2000000,
                    "transaction_requirements":[
                        "expected_network_fee_lamports":"10000", "maximum_network_fee_lamports":"20000",
                        "account_funding_lamports":"1488440", "nonrefundable_account_cost_lamports":"0",
                        "available_sol_lamports":"7476550091", "required_starting_sol_lamports":"11498440",
                        "remaining_sol_after_requirements_lamports":"7465051651", "funding_within_approved_cap":true,
                    ],
                ] as [String: Any],
                "financial_intent":["expected":[expected], "maximum_costs":[feeCost]] as [String: Any],
            ]
        }
        let meteoraDlmm = financialAuthenticationReason(meteoraSwapBinding(family:"dlmm"), summary:meteoraSummary)!
        for detail in ["0.01 SOL", "at least 1.059423 USDC", "Meteora DLMM", "pool fee (included) 0.000004 SOL", "network fee 0.00001 SOL (max 0.00002)", "account funding 0.00148844 SOL (max 0.002)"] {
            assert(meteoraDlmm.contains(detail), detail)
        }
        assert(meteoraDlmm == "allow swapping 0.01 SOL for at least 1.059423 USDC via Meteora DLMM on Solana; pool fee (included) 0.000004 SOL; network fee 0.00001 SOL (max 0.00002); account funding 0.00148844 SOL (max 0.002); available 7.476550091 SOL.")
        assert(meteoraDlmm.count <= authenticationReasonMaximumCharacters)
        assert(!meteoraDlmm.contains("reviewed operation") && !meteoraDlmm.contains("terminal review"))
        var longMeteoraBinding = meteoraSwapBinding(family:"dlmm")
        var longMeteoraIntent = longMeteoraBinding["financial_intent"] as! [String: Any]
        var longMeteoraExpected = longMeteoraIntent["expected"] as! [[String: Any]]
        longMeteoraExpected[0]["maximum_input"] = "123456789012345678"
        longMeteoraExpected[0]["minimum_output"] = "123456789012345678"
        longMeteoraIntent["expected"] = longMeteoraExpected
        longMeteoraBinding["financial_intent"] = longMeteoraIntent
        let longMeteoraReason = financialAuthenticationReason(longMeteoraBinding, summary:meteoraSummary)!
        assert(longMeteoraReason.count <= authenticationReasonMaximumCharacters)
        for detail in ["123456789.012345678 SOL", "at least 123456789012.345678 USDC",
                       "Meteora DLMM", "pool fee (included) 0.000004 SOL",
                       "network fee 0.00001 SOL (max 0.00002)",
                       "account funding 0.00148844 SOL (max 0.002)"] {
            assert(longMeteoraReason.contains(detail), detail)
        }
        assert(!longMeteoraReason.contains("available"))
        let meteoraDamm = financialAuthenticationReason(meteoraSwapBinding(family:"damm_v2"), summary:meteoraSummary)!
        assert(meteoraDamm.contains("Meteora DAMM v2"))
        let meteoraWsol = financialAuthenticationReason(meteoraSwapBinding(family:"dlmm", inputMint:wsolMint), summary:meteoraSummary)!
        assert(meteoraWsol.contains("0.01 WSOL") && meteoraWsol.contains("pool fee (included) 0.000004 WSOL"))
        assert(!meteoraWsol.contains("0.000004 SOL"))
        var mismatchedMeteora = meteoraSwapBinding(family:"dlmm")
        var meteoraExecution = mismatchedMeteora["execution"] as! [String: Any]
        var meteoraCandidate = meteoraExecution["protocol_candidate"] as! [String: Any]
        var meteoraQuote = meteoraCandidate["quote"] as! [String: Any]
        meteoraQuote["pool"] = "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie"
        meteoraCandidate["quote"] = meteoraQuote
        meteoraExecution["protocol_candidate"] = meteoraCandidate
        mismatchedMeteora["execution"] = meteoraExecution
        assert(financialAuthenticationReason(mismatchedMeteora, summary:meteoraSummary) == nil)

        func nativeSolBinding(fundingLamports: Int) -> [String: Any] {
            let usdcAsset: [String: Any] = ["network":"solana:mainnet-beta",
                "reference":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "decimals":6]
            var expected: [[String: Any]] = [["kind":"conversion", "resource":solResource,
                "input":solAsset, "maximum_input":"100000000", "output":usdcAsset,
                "minimum_output":"9900000"]]
            var debits: [[String: Any]] = [["resource":solResource,
                "amount":["asset":solAsset, "atoms":"100000000"]]]
            if fundingLamports > 0 {
                expected.append(["kind":"debit", "resource":solResource,
                    "asset":solAsset, "maximum_debit":String(fundingLamports)])
                debits.append(["resource":solResource,
                    "amount":["asset":solAsset, "atoms":String(fundingLamports)]])
            }
            return ["version":1, "operation":"jupiter.swap",
                "execution":["wallet_address":solWallet, "input_atoms":"100000000",
                    "minimum_output_atoms":"9900000", "maximum_network_fee_lamports":"1000000",
                    "account_funding_lamports":fundingLamports,
                    "funding_accounts":fundingLamports > 0 ? [["payer":solWallet,"lamports":fundingLamports]] : [],
                    "platform_fee_bps":0] as [String: Any],
                "financial_intent":["expected":expected, "debits":debits,
                    "maximum_costs":[["location":["kind":"cash", "resource":solResource],
                        "amount":["asset":solAsset, "atoms":"1000000"],
                        "treatment":"additional_debit"]]] as [String: Any]]
        }
        let nativeSolNoFunding = nativeSolBinding(fundingLamports:0)
        let nativeSolSummary = "Swap 100000000 atoms of native SOL for at least 9900000 atoms of USDC on Solana mainnet-beta."
        assert(financialAuthenticationReason(nativeSolNoFunding, summary:nativeSolSummary)?.contains("0.1 SOL for at least 9.9 USDC") == true)
        let nativeSolWithFunding = nativeSolBinding(fundingLamports:1_488_440)
        assert(financialAuthenticationReason(nativeSolWithFunding, summary:nativeSolSummary)?.contains("account funding up to 0.00148844 SOL") == true)
        var sponsoredNativeSol = nativeSolBinding(fundingLamports:1_488_440)
        var sponsoredExecution = sponsoredNativeSol["execution"] as! [String: Any]
        sponsoredExecution["funding_accounts"] = [["payer":"11111111111111111111111111111112", "lamports":1_488_440]]
        sponsoredNativeSol["execution"] = sponsoredExecution
        var sponsoredIntent = sponsoredNativeSol["financial_intent"] as! [String: Any]
        sponsoredIntent["debits"] = Array((sponsoredIntent["debits"] as! [[String: Any]]).dropLast())
        sponsoredIntent["expected"] = (sponsoredIntent["expected"] as! [[String: Any]]).filter { $0["kind"] as? String != "debit" }
        sponsoredNativeSol["financial_intent"] = sponsoredIntent
        assert(financialAuthenticationReason(sponsoredNativeSol, summary:nativeSolSummary)?.contains("account funding 0.00148844 SOL total, 0 SOL from your wallet") == true)

        let forgedJupiterSummary = "Swap 99 scam-token for at least 1 BONK; max network fee 99 SOL; account funding up to 99 SOL."
        let forgedJupiterReason = financialAuthenticationReason(jupiterBinding, summary:forgedJupiterSummary)!
        assert(forgedJupiterReason == jupiterReason)
        var mismatchJupiter = jupiterBinding
        var mismatchIntent = jupiterBinding["financial_intent"] as! [String: Any]
        var mismatchExpected = mismatchIntent["expected"] as! [[String: Any]]
        mismatchExpected[0]["minimum_output"] = "9876543210001"
        mismatchIntent["expected"] = mismatchExpected
        mismatchJupiter["financial_intent"] = mismatchIntent
        assert(financialAuthenticationReason(mismatchJupiter, summary:jupiterRawSummary) == nil)
        mismatchJupiter = jupiterBinding
        var mismatchExecution = jupiterBinding["execution"] as! [String: Any]
        mismatchExecution["maximum_network_fee_lamports"] = "1000001"
        mismatchJupiter["execution"] = mismatchExecution
        assert(financialAuthenticationReason(mismatchJupiter, summary:jupiterRawSummary) == nil)
        mismatchJupiter = jupiterBinding
        mismatchExecution = jupiterBinding["execution"] as! [String: Any]
        mismatchExecution["account_funding_lamports"] = 5000001
        mismatchJupiter["execution"] = mismatchExecution
        assert(financialAuthenticationReason(mismatchJupiter, summary:jupiterRawSummary) == nil)
        mismatchJupiter = jupiterBinding
        mismatchIntent = jupiterBinding["financial_intent"] as! [String: Any]
        mismatchExpected = mismatchIntent["expected"] as! [[String: Any]]
        var wrongOutput = mismatchExpected[0]["output"] as! [String: Any]
        wrongOutput["network"] = "solana:devnet"
        mismatchExpected[0]["output"] = wrongOutput
        mismatchIntent["expected"] = mismatchExpected
        mismatchJupiter["financial_intent"] = mismatchIntent
        assert(financialAuthenticationReason(mismatchJupiter, summary:jupiterRawSummary) == nil)

        var unknownMintAsset = bpAsset
        unknownMintAsset["reference"] = "11111111111111111111111111111112"
        unknownMintAsset["decimals"] = 0
        mismatchJupiter = jupiterBinding
        mismatchIntent = jupiterBinding["financial_intent"] as! [String: Any]
        mismatchExpected = mismatchIntent["expected"] as! [[String: Any]]
        mismatchExpected[0]["input"] = unknownMintAsset
        mismatchExpected[0]["maximum_input"] = "1234"
        mismatchIntent["expected"] = mismatchExpected
        var unknownDebits = mismatchIntent["debits"] as! [[String: Any]]
        unknownDebits[0]["amount"] = ["asset":unknownMintAsset, "atoms":"1234"]
        mismatchIntent["debits"] = unknownDebits
        mismatchJupiter["financial_intent"] = mismatchIntent
        mismatchExecution = jupiterBinding["execution"] as! [String: Any]
        mismatchExecution["input_atoms"] = "1234"
        mismatchJupiter["execution"] = mismatchExecution
        let unknownReason = financialAuthenticationReason(mismatchJupiter, summary:jupiterRawSummary)!
        assert(unknownReason.contains("1234 11111111111111111111111111111112"))
        var nativeExecution = swapBinding["execution"] as! [String: Any]
        nativeExecution["token"] = promptNativeAsset
        nativeExecution["units"] = "1000000000000000"
        nativeExecution["minimum_output"] = "2489004"
        var nativeSwapBinding = swapBinding
        nativeSwapBinding["execution"] = nativeExecution
        let nativeSwapSummary = "Swap 0.001 ETH for at least 2.489004 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v3 fee 500 ppm; router 0x2222222222222222222222222222222222222222; deadline \(futureDeadline); max network fee 0.001 ETH."
        let nativeSwapReason = financialAuthenticationReason(nativeSwapBinding, summary: nativeSwapSummary)!
        assert(nativeSwapReason.contains("allow swapping 0.001 ETH for at least 2.489004 USDC on Ethereum"))
        assert(!nativeSwapReason.contains("permit") && nativeSwapReason.count <= 300)
        let forgedSummary = swapSummary
            .replacingOccurrences(of: "Swap 0.1 WETH for at least 200 USDC", with: "Swap 99 WETH for at least 1 USDC")
            .replacingOccurrences(of: "max network fee 0.001 ETH", with: "max network fee 99 ETH")
        let structuredReason = financialAuthenticationReason(swapBinding, summary: forgedSummary)!
        assert(structuredReason.contains("allow swapping 0.1 WETH for at least 200 USDC"))
        assert(structuredReason.contains("max network fees 0.001 ETH") && !structuredReason.contains("99 ETH"))
        var ramsesBinding = swapBinding
        ramsesBinding["operation"] = "ramses.swap"
        var ramsesExecution = swapBinding["execution"] as! [String: Any]
        ramsesExecution["permit_template"] = ["typed":"synthetic"]
        ramsesBinding["execution"] = ramsesExecution
        let ramsesSummary = "Swap 0.1 TOKEN for at least 190 USDG; recipient 0x1111111111111111111111111111111111111111; Ramses tick spacing 10, quoted pool fee 500 ppm (quote-time ceiling 1000 ppm only); router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline \(futureDeadline); max network fee 0.001 ETH; permission: finite token allowance or typed EIP-2612 permit to Ramses router only after approval."
        let ramsesReason = financialAuthenticationReason(ramsesBinding, summary: ramsesSummary)!
        for detail in ["allow swapping 0.1 TOKEN for at least 200 USDG", "max network fees 0.001 ETH", "limited to this swap"] {
            assert(ramsesReason.contains(detail), detail)
        }
        assert(ramsesReason.count <= 300 && !ramsesReason.contains("0x") && !ramsesReason.contains("pool"))
        // Synthetic protocol fixtures exercise presentation only; no Touch ID.
        for (operation, dex, brand, tier) in [("sushi.swap", "sushi", "Sushi", 3000), ("pancakeswap.swap", "pancakeswap", "PancakeSwap", 2500)] {
            var forkBinding = swapBinding
            forkBinding["operation"] = operation
            var execution = swapBinding["execution"] as! [String: Any]
            execution["dex"] = dex; execution["protocol"] = "v3"; execution["fee_tier"] = tier
            execution["factory"] = "0x4444444444444444444444444444444444444444"
            execution["pool"] = "0x3333333333333333333333333333333333333333"
            execution["spender"] = "0x2222222222222222222222222222222222222222"
            execution["transaction"] = ["to":execution["spender"]!, "chain_id":1]
            if dex == "pancakeswap" { execution["pool_deployer"] = "0x5555555555555555555555555555555555555555" }
            forkBinding["execution"] = execution
            let reason = financialAuthenticationReason(forkBinding, summary:swapSummary)!
            for detail in ["0.1 WETH", "at least 200 USDC", "on Ethereum via \(brand)", "max network fees 0.001 ETH", "expires in "] { assert(reason.contains(detail), detail) }
            assert(reason.count <= 300 && !reason.contains("0x") && !reason.contains("ppm"))
            assert(nativeApprovalReasonForOS(reason + ".") == String(reason.dropLast()))
            for field in ["dex", "factory", "pool", "transaction"] {
                var incomplete = execution; incomplete.removeValue(forKey:field)
                forkBinding["execution"] = incomplete
                assert(financialAuthenticationReason(forkBinding, summary:swapSummary) == nil, field)
            }
            for (field, value) in [("dex", "ramses" as Any), ("protocol", "v4" as Any), ("tick_spacing", 10 as Any), ("max_pool_fee_ppm", 1000 as Any), ("fee_tier", (dex == "sushi" ? 2500 : 3000) as Any)] {
                var invalid = execution; invalid[field] = value; forkBinding["execution"] = invalid
                assert(financialAuthenticationReason(forkBinding, summary:swapSummary) == nil, field)
            }
            for map in [[String(tier):execution["pool"]!] as Any, NSNull() as Any] {
                var mappedExecution = execution; mappedExecution["approved_pools"] = map; forkBinding["execution"] = mappedExecution
                assert(financialAuthenticationReason(forkBinding, summary:swapSummary) == nil)
                mappedExecution.removeValue(forKey:"pool"); forkBinding["execution"] = mappedExecution
                assert(financialAuthenticationReason(forkBinding, summary:swapSummary) == nil)
            }
            var zeroPool = execution; zeroPool["pool"] = "0x" + String(repeating:"0", count:40)
            forkBinding["execution"] = zeroPool
            assert(financialAuthenticationReason(forkBinding, summary:swapSummary) == nil)
            if dex == "pancakeswap" {
                var noDeployer = execution; noDeployer.removeValue(forKey:"pool_deployer"); forkBinding["execution"] = noDeployer
                assert(financialAuthenticationReason(forkBinding, summary:swapSummary) == nil)
            }
        }
        var permitExecution = swapBinding["execution"] as! [String: Any]
        permitExecution["permit_template"] = ["typed":"synthetic"]
        var permitBinding = swapBinding
        permitBinding["execution"] = permitExecution
        let permitSummary = "Swap 0.1 WETH for at least 200 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v4 fee 500 ppm, tick spacing 10; router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline \(futureDeadline); max network fee 0.001 ETH; permission: finite token allowance to canonical Permit2 was checked; a typed Permit2 signature will authorize the router only after approval."
        let permitReason = financialAuthenticationReason(permitBinding, summary: permitSummary)!
        for detail in ["allow swapping 0.1 WETH for at least 200 USDC", "max network fees 0.001 ETH", "limited to this swap"] {
            assert(permitReason.contains(detail), detail)
        }
        let eip2612Summary = "Swap 0.1 WETH for at least 200 USDC; recipient 0x1111111111111111111111111111111111111111; Uniswap v3 fee 3000 ppm; router 0x2222222222222222222222222222222222222222; slippage 50 bps, impact max 40 bps; deadline \(futureDeadline); max network fee 0.001 ETH; permission: typed EIP-2612 permit authorizes router only after approval."
        let eip2612Reason = financialAuthenticationReason(permitBinding, summary: eip2612Summary)!
        for detail in ["allow swapping 0.1 WETH for at least 200 USDC", "max network fees 0.001 ETH", "limited to this swap"] {
            assert(eip2612Reason.contains(detail), detail)
        }
        let unsafeLabelSummary = "Swap 1 0x1111111111111111111111111111111111111111 for at least 2 0x2222222222222222222222222222222222222222; deadline \(futureDeadline); max network fee 0.001 ETH."
        let safeFallbackReason = financialAuthenticationReason(swapBinding, summary: unsafeLabelSummary)!
        assert(safeFallbackReason.contains("allow swapping 0.1 token for at least 200 token"))
        assert(!safeFallbackReason.contains("0x"))
        var expiredBinding = swapBinding
        var expiredExecution = swapBinding["execution"] as! [String: Any]
        expiredExecution["deadline"] = 1
        expiredBinding["execution"] = expiredExecution
        assert(financialAuthenticationReason(expiredBinding, summary: swapSummary) == nil)
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
        assert(nativeApprovalReasonForOS(boundaryAuthenticationReason(boundary, summary:boundarySummary)! + ".") == "approve these boundaries: \(boundarySummary.dropLast())")
        assert(financialAuthenticationReason(boundary, summary:boundarySummary) == nil)
        var bridgeBoundary = boundary
        bridgeBoundary["operation"] = "bridge.transfer"
        let walletAddress = "0x251500000000000000000000000000000000bd1c"
        bridgeBoundary["fixed"] = ["source_address": walletAddress, "refund_to": walletAddress]
        let bridgeGrantSummary = "Bridge 0.004 ETH; evm-dev Ethereum Mainnet → Arbitrum One via Relay; recipient evm-dev; Receive ≥0.0039 ETH; Cumulative max fees 0.0001 ETH gas, 0.00004 ETH withheld from output; Total fees up to 0.00014 ETH. Finite grant covers these declared steps: deposit source input 0.004 ETH."
        let bridgeGrantReason = boundaryAuthenticationReason(bridgeBoundary, summary: bridgeGrantSummary)!
        assert(bridgeGrantReason == "bridge 0.004 ETH from Ethereum to Arbitrum. You’ll get at least 0.0039 ETH. Total fees won’t exceed 0.00014 ETH.")
        assert(!bridgeGrantReason.contains("boundaries") && !bridgeGrantReason.contains("evm-dev") && !bridgeGrantReason.contains("Relay"))
        let solanaUSDCGrantSummary = "Bridge 5 USDC; solana-dev Solana mainnet-beta → Ethereum Mainnet via Relay; recipient evm-dev; Receive ≥4.8 USDC; Cumulative max fees 0.0003 SOL gas, 0.003 SOL additional fees, 0.1 USDC withheld from output; Total fees up to 0.0033 SOL and 0.1 USDC; Refund paths Solana mainnet-beta 9WzQ…v5zH ≥0 USDC, Ethereum Mainnet 0x2515…bd1c ≥0 USDC. Finite grant covers these declared steps: deposit source input 5 USDC."
        let solanaUSDCGrantReason = boundaryAuthenticationReason(bridgeBoundary, summary: solanaUSDCGrantSummary)!
        assert(solanaUSDCGrantReason.contains("bridge 5 USDC from Solana to Ethereum"))
        assert(solanaUSDCGrantReason.contains("recipient evm-dev"))
        assert(solanaUSDCGrantReason.contains("4.8 USDC") && solanaUSDCGrantReason.contains("0.0033 SOL and 0.1 USDC"))
        assert(solanaUSDCGrantReason.contains("refunds may be zero"))
        assert(!solanaUSDCGrantReason.contains("EPjFWdd5"))
        let ethereumToSolanaSummary = "Bridge 5 USDC; evm-dev Ethereum Mainnet → Solana mainnet-beta via Relay; recipient solana-dev; Receive ≥4.8 USDC; Total fees up to $0.36."
        let ethereumToSolanaReason = boundaryAuthenticationReason(bridgeBoundary, summary: ethereumToSolanaSummary)!
        assert(ethereumToSolanaReason.contains("bridge 5 USDC from Ethereum to Solana"))
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
        let swapBoundarySummary = "Swap up to 0.1 WETH for USDC; maximum native fee 0.001 ETH, slippage 100 bps, price impact 40 bps, deadline 600 seconds."
        let swapBoundaryReason = boundaryAuthenticationReason(swapBoundary, summary:swapBoundarySummary)!
        for detail in ["allow swapping up to 0.1 WETH for at least 190 USDC on Ethereum for 10 minutes",
                       "max network fees 0.001 ETH", "limited to each swap"] {
            assert(swapBoundaryReason.contains(detail), detail)
        }
        for hidden in ["0x", "router", "atoms", "bps", "1791500000"] { assert(!swapBoundaryReason.contains(hidden), hidden) }
        assert(swapBoundaryReason.count <= 300)
        let unsafeBoundaryLabels = boundaryAuthenticationReason(
            swapBoundary,
            summary: "Swap up to 0.1 0x0000000000000000000000000000000000000064 for 0x0000000000000000000000000000000000000065;"
        )!
        assert(unsafeBoundaryLabels.contains("up to 0.1 token for at least 190 token"))
        assert(!unsafeBoundaryLabels.contains("0x"))
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
        let ramsesBoundaryReason = boundaryAuthenticationReason(ramsesBoundary, summary: "Swap up to 0.1 TOKEN for USDG; Ramses v3 bounded swap summary")!
        for detail in ["allow swapping up to 0.1 TOKEN for at least 190 USDG on Ethereum for 10 minutes",
                       "max network fees 0.001 ETH"] {
            assert(ramsesBoundaryReason.contains(detail), detail)
        }
        for hidden in ["0x", "pool", "factory", "ppm", "atoms", "bps"] { assert(!ramsesBoundaryReason.contains(hidden), hidden) }
        assert(ramsesBoundaryReason.count <= 300)
        for (operation, dex, brand, tier) in [("sushi.swap", "sushi", "Sushi", 3000), ("pancakeswap.swap", "pancakeswap", "PancakeSwap", 2500)] {
            var forkBoundary = swapBoundary; forkBoundary["operation"] = operation
            var fixed = swapBoundary["fixed"] as! [String: Any]
            fixed["operation"] = operation; fixed["dex"] = dex
            fixed["factory"] = "0x4444444444444444444444444444444444444444"
            fixed["pool"] = "0x3333333333333333333333333333333333333333"
            fixed["pool_deployer"] = dex == "pancakeswap" ? "0x5555555555555555555555555555555555555555" as Any : NSNull()
            var parameters = swapBoundary["parameters"] as! [String: Any]; parameters["allowed_fee_tiers"] = [tier]
            forkBoundary["fixed"] = fixed; forkBoundary["parameters"] = parameters
            let reason = boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary)!
            assert(reason.contains("at least 190 USDC on Ethereum via \(brand) for 10 minutes"))
            assert(reason.contains("max network fees 0.001 ETH") && reason.count <= 300 && !reason.contains("0x"))
            var legacyMultiple = parameters; legacyMultiple["allowed_fee_tiers"] = [500, tier]
            forkBoundary["parameters"] = legacyMultiple
            assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == reason)
            var mappedFixed = fixed; mappedFixed.removeValue(forKey:"pool")
            mappedFixed["approved_pools"] = [String(tier):fixed["pool"]!]
            forkBoundary["fixed"] = mappedFixed; forkBoundary["parameters"] = parameters
            assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == reason)
            var allParameters = parameters; allParameters["allowed_fee_tiers"] = [100, 500, tier, 10000]
            var allFixed = mappedFixed
            allFixed["approved_pools"] = ["100":"0x1111111111111111111111111111111111111111", "500":"0x6666666666666666666666666666666666666666", String(tier):fixed["pool"]!, "10000":"0x7777777777777777777777777777777777777777"]
            forkBoundary["fixed"] = allFixed; forkBoundary["parameters"] = allParameters
            assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == reason)
            for (label, invalidFixed, invalidParameters) in invalidPoolMapScopes(fixed:mappedFixed, parameters:parameters, tier:tier, dex:dex) {
                forkBoundary["fixed"] = invalidFixed; forkBoundary["parameters"] = invalidParameters
                assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == nil, label)
            }
            forkBoundary["parameters"] = parameters
            for field in ["dex", "factory", "pool", "router"] {
                var incomplete = fixed; incomplete.removeValue(forKey:field); forkBoundary["fixed"] = incomplete
                assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == nil, field)
            }
            for (field, value) in [("dex", "uniswap" as Any), ("protocol", "v4" as Any), ("tick_spacing", 10 as Any)] {
                var invalid = fixed; invalid[field] = value; forkBoundary["fixed"] = invalid
                assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == nil, field)
            }
            forkBoundary["fixed"] = fixed
            parameters["allowed_fee_tiers"] = [dex == "sushi" ? 2500 : 3000]; forkBoundary["parameters"] = parameters
            assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == nil)
            parameters["allowed_fee_tiers"] = [tier]; parameters["max_pool_fee_ppm"] = 1000; forkBoundary["parameters"] = parameters
            assert(boundaryAuthenticationReason(forkBoundary, summary:swapBoundarySummary) == nil)
        }
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
        assert(financialPlanAuthenticationReason(planBinding, summary:"Full swap scope is in the terminal review.") == "approve the plan “Synthetic rotation” (1 step, 10 minutes)")
        let financialSummary = "Send 0.01 ETH to Savings; fees up to 0.001 ETH."
        var summarizedPlan = planBinding
        summarizedPlan["plan"] = ["name":"Synthetic rotation", "summary":financialSummary, "steps":[planStep]]
        let summarizedReason = "approve the plan “Synthetic rotation”. \(financialSummary) (1 step, 10 minutes)"
        assert(financialPlanAuthenticationReason(summarizedPlan, summary:"Broker scope") == summarizedReason)
        assert(nativeApprovalReasonForOS(summarizedReason) == summarizedReason)
        summarizedPlan["replaces_revision_id"] = "synthetic-prior-revision"
        assert(financialPlanAuthenticationReason(summarizedPlan, summary:"Broker scope") == "approve the revised plan “Synthetic rotation”. \(financialSummary) (1 step, 10 minutes)")
        let invalidPlanSummaries: [Any] = ["", " leading", "trailing ", "line\nbreak", "tab\there", "escape\u{1b}",
                                   "line\u{2028}break", "paragraph\u{2029}break", "hidden\u{202e}amount",
                                   "Arabic\u{061c}amount", "isolate\u{2066}amount\u{2069}",
                                   String(repeating: "界", count: 140), true, NSNull()]
        for invalidSummary in invalidPlanSummaries {
            var invalid = planBinding
            invalid["plan"] = ["name":"Synthetic rotation", "summary":invalidSummary, "steps":[planStep]]
            assert(financialPlanAuthenticationReason(invalid, summary:"Broker scope") == nil)
        }
        var maximumSummaryPlan = planBinding
        let maximumSummary = String(repeating: "界", count: 140 - "Synthetic rotation".unicodeScalars.count)
        maximumSummaryPlan["plan"] = ["name":"Synthetic rotation", "summary":maximumSummary, "steps":[planStep]]
        maximumSummaryPlan["replaces_revision_id"] = "synthetic-prior-revision"
        maximumSummaryPlan["valid_for_seconds"] = NSNumber(value: UInt64.max)
        let maximumSummaryReason = financialPlanAuthenticationReason(maximumSummaryPlan, summary:"Broker scope")!
        assert(maximumSummaryReason.contains(maximumSummary))
        assert(maximumSummaryReason.contains(approvalDuration(UInt64.max)))
        assert(maximumSummaryReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
        var generatedPermission = planPermission; generatedPermission["generated_input_fields"] = ["/requestId"]
        var generatedPlan = planBinding; generatedPlan["permissions"] = [generatedPermission]
        assert(financialPlanAuthenticationReason(generatedPlan, summary:"Full scope is in the terminal review.") == "approve the plan “Synthetic rotation” (1 step, 10 minutes)")
        generatedPermission["generated_input_fields"] = "requestId"
        generatedPlan["permissions"] = [generatedPermission]
        assert(financialPlanAuthenticationReason(generatedPlan, summary:"review") == nil)
        var revisedPlan = planBinding; revisedPlan["replaces_revision_id"] = "synthetic-prior-revision"
        assert(financialPlanAuthenticationReason(revisedPlan, summary:"Full scope is in the terminal review.") == "approve the revised plan “Synthetic rotation” (1 step, 10 minutes)")
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
                                      "summary":"Swap up to 0.1 WETH for USDC; maximum native fee 0.001 ETH, slippage 100 bps, price impact 40 bps, deadline 600 seconds."]
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
        // Exact AMM plans use the same frozen schema/connection hashes as
        // other exact steps; they have no bounded adapter permission spec.
        var exactAMMPlan = ammPlan
        var exactAMMBody = ammPlan["plan"] as! [String: Any]
        var exactAMMStep = swapPlanStep
        let exactAMMScope: [String: Any] = ["mode":"exact",
            "maximum_debits":ammSpec["maximum_debits"]!, "maximum_costs":ammSpec["maximum_costs"]!,
            "possible_credits":[["resource":"ethereum:1:\(signerAddress)", "asset":outputAsset]]]
        exactAMMStep["permitted_variations"] = exactAMMScope
        exactAMMBody["steps"] = [exactAMMStep]
        exactAMMPlan["plan"] = exactAMMBody
        let exactAMMPermission: [String: Any] = ["step_id":"swap", "exact":["scope":exactAMMScope,
            "definition_hash":"SYNTHETIC-schema", "connections_hash":"SYNTHETIC-connections"]]
        exactAMMPlan["permissions"] = [exactAMMPermission]
        let exactAMMSummary = "Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds."
        let exactAMMReason = "approve the plan “Synthetic bounded swap” (1 step, 10 minutes)"
        assert(financialPlanAuthenticationReason(exactAMMPlan, summary:exactAMMSummary) == exactAMMReason)
        func exactAMMWith(step: [String: Any] = exactAMMStep,
                          permission: [String: Any] = exactAMMPermission) -> [String: Any] {
            var result = exactAMMPlan
            var body = exactAMMBody; body["steps"] = [step]
            result["plan"] = body; result["permissions"] = [permission]
            return result
        }
        var relativeExactStep = exactAMMStep
        var relativeExactInput = swapInput; relativeExactInput.removeValue(forKey:"deadline")
        relativeExactStep["input"] = relativeExactInput
        relativeExactStep["action_deadline_seconds"] = 600
        assert(financialPlanAuthenticationReason(exactAMMWith(step:relativeExactStep), summary:exactAMMSummary) == exactAMMReason)
        for invalidWindow: Any in [0, -1, 601, true, "600", 1.5, NSNull()] {
            var invalid = relativeExactStep; invalid["action_deadline_seconds"] = invalidWindow
            assert(financialPlanAuthenticationReason(exactAMMWith(step:invalid), summary:exactAMMSummary) == nil)
        }
        for conflictingDeadline: Any in [1791500000, NSNull()] {
            var invalid = relativeExactStep; var input = relativeExactInput
            input["deadline"] = conflictingDeadline; invalid["input"] = input
            assert(financialPlanAuthenticationReason(exactAMMWith(step:invalid), summary:exactAMMSummary) == nil)
        }
        for field in ["scope", "definition_hash", "connections_hash"] {
            var permission = exactAMMPermission; var exact = permission["exact"] as! [String: Any]
            exact.removeValue(forKey:field); permission["exact"] = exact
            assert(financialPlanAuthenticationReason(exactAMMWith(permission:permission), summary:exactAMMSummary) == nil)
        }
        for field in ["maximum_debits", "maximum_costs", "possible_credits"] {
            var malformedScope = exactAMMScope; malformedScope[field] = "SYNTHETIC-invalid-array"
            var step = exactAMMStep; step["permitted_variations"] = malformedScope
            var permission = exactAMMPermission; var exact = permission["exact"] as! [String: Any]
            exact["scope"] = malformedScope; permission["exact"] = exact
            assert(financialPlanAuthenticationReason(exactAMMWith(step:step, permission:permission), summary:exactAMMSummary) == nil)
        }
        for operation in ["uniswap.swap", "ramses.swap", "sushi.swap", "pancakeswap.swap"] {
            var exactVenueStep = exactAMMStep; exactVenueStep["operation"] = operation
            assert(financialPlanAuthenticationReason(exactAMMWith(step:exactVenueStep), summary:exactAMMSummary) == exactAMMReason)
        }
        var mismatchedExactStep = exactAMMStep
        var mismatchedExactScope = exactAMMScope; mismatchedExactScope["maximum_debits"] = [[String: Any]]()
        mismatchedExactStep["permitted_variations"] = mismatchedExactScope
        assert(financialPlanAuthenticationReason(exactAMMWith(step:mismatchedExactStep), summary:exactAMMSummary) == nil)
        var ambiguousAMMPermission = exactAMMPermission
        ambiguousAMMPermission["permission"] = ammPlanPermission["permission"]
        assert(financialPlanAuthenticationReason(exactAMMWith(permission:ambiguousAMMPermission), summary:exactAMMSummary) == nil)
        assert(financialPlanAuthenticationReason(exactAMMPlan, summary:"Different broker scope") == nil)
        // One concise prompt supports mixed permission variants and venues.
        var boundedSecondStep = swapPlanStep; boundedSecondStep["step_id"] = "bounded-swap"
        var boundedSecondPermission = ammPlanPermission; boundedSecondPermission["step_id"] = "bounded-swap"
        var mixedAMMPlan = exactAMMPlan; var mixedAMMBody = exactAMMBody
        mixedAMMBody["steps"] = [exactAMMStep, boundedSecondStep]
        mixedAMMPlan["plan"] = mixedAMMBody
        mixedAMMPlan["permissions"] = [exactAMMPermission, boundedSecondPermission]
        assert(financialPlanAuthenticationReason(mixedAMMPlan,
            summary:"Approve the plan “Synthetic bounded swap” with 2 ordered steps for 600 seconds.")
            == "approve the plan “Synthetic bounded swap” (2 steps, 10 minutes)")
        var krakenSecondStep = planStep; krakenSecondStep["operation"] = "kraken.add-order"
        krakenSecondStep["step_id"] = "kraken-buy"
        var krakenSecondPermission = planPermission; krakenSecondPermission["step_id"] = "kraken-buy"
        mixedAMMBody["steps"] = [relativeExactStep, krakenSecondStep]
        mixedAMMPlan["plan"] = mixedAMMBody
        mixedAMMPlan["permissions"] = [exactAMMPermission, krakenSecondPermission]
        mixedAMMPlan["valid_for_seconds"] = 2700
        assert(financialPlanAuthenticationReason(mixedAMMPlan,
            summary:"Approve the plan “Synthetic bounded swap” with 2 ordered steps for 2700 seconds.")
            == "approve the plan “Synthetic bounded swap” (2 steps, 45 minutes)")
        let ammPlanReason = financialPlanAuthenticationReason(ammPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.")!
        assert(ammPlanReason == "approve the plan “Synthetic bounded swap” (1 step, 10 minutes)")
        var summarizedAMMPlan = ammPlan
        var summarizedAMMSpec = ammPlan["plan"] as! [String: Any]
        summarizedAMMSpec["summary"] = "Swap up to 0.1 WETH for at least 190 USDC; gas up to 0.001 ETH."
        summarizedAMMPlan["plan"] = summarizedAMMSpec
        assert(financialPlanAuthenticationReason(summarizedAMMPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.") == "approve the plan “Synthetic bounded swap”. Swap up to 0.1 WETH for at least 190 USDC; gas up to 0.001 ETH. (1 step, 10 minutes)")
        assert(financialPlanAuthenticationReason(summarizedAMMPlan, summary:"Different broker scope") == nil)
        // Future-funded swaps bind a relative window, not an already-ticking
        // absolute deadline. Exercise the same parser used before Touch ID.
        func relativePlan(_ base: [String: Any], window: Any = 600,
                          absolute: Any? = nil) -> [String: Any] {
            var result = base
            var body = result["plan"] as! [String: Any]
            var steps = body["steps"] as! [[String: Any]]
            var input = steps[0]["input"] as! [String: Any]
            input["deadline"] = absolute
            steps[0]["input"] = input
            steps[0]["action_deadline_seconds"] = window
            body["steps"] = steps
            result["plan"] = body
            result["valid_for_seconds"] = 2700
            return result
        }
        let relativeSummary = "Approve the plan “Synthetic bounded swap” with 1 ordered steps for 2700 seconds."
        let relativeReason = "approve the plan “Synthetic bounded swap”. Swap up to 0.1 WETH for at least 190 USDC; gas up to 0.001 ETH. (1 step, 45 minutes)"
        let relativeAMMPlan = relativePlan(summarizedAMMPlan)
        assert(financialPlanAuthenticationReason(relativeAMMPlan, summary:relativeSummary) == relativeReason)
        var revisedRelative = relativeAMMPlan
        revisedRelative["replaces_revision_id"] = "synthetic-prior-relative-revision"
        assert(financialPlanAuthenticationReason(revisedRelative, summary:relativeSummary) == relativeReason.replacingOccurrences(of:"approve the plan", with:"approve the revised plan"))
        for invalidWindow: Any in [0, -1, 601, true, "600", 1.5] {
            assert(financialPlanAuthenticationReason(relativePlan(summarizedAMMPlan, window:invalidWindow), summary:relativeSummary) == nil)
        }
        for conflictingDeadline: Any in [1791500000, NSNull()] {
            assert(financialPlanAuthenticationReason(relativePlan(summarizedAMMPlan, absolute:conflictingDeadline), summary:relativeSummary) == nil)
        }
        assert(financialPlanAuthenticationReason(relativePlan(summarizedAMMPlan, window:NSNull()), summary:relativeSummary) == nil)
        // Null optional window retains the legacy absolute-deadline contract.
        assert(financialPlanAuthenticationReason(relativePlan(summarizedAMMPlan, window:NSNull(), absolute:1791500000), summary:relativeSummary) == relativeReason)
        var tighterRelativePlan = relativeAMMPlan
        var tighterSpec = ammSpec
        var tighterBounds = ammSpec["parameters"] as! [String: Any]
        tighterBounds["max_deadline_seconds"] = 300
        tighterSpec["parameters"] = tighterBounds
        var tighterPermission = ammPlanPermission
        var tighterBody = ammPlanPermission["permission"] as! [String: Any]
        tighterBody["spec"] = tighterSpec
        tighterPermission["permission"] = tighterBody
        tighterRelativePlan["permissions"] = [tighterPermission]
        assert(financialPlanAuthenticationReason(tighterRelativePlan, summary:relativeSummary) == nil)
        assert(financialPlanAuthenticationReason(relativePlan(tighterRelativePlan, window:300), summary:relativeSummary) == relativeReason)
        assert(nativeApprovalReasonForOS(ammPlanReason + ".") == ammPlanReason)
        var revisedAMMPlan = ammPlan; revisedAMMPlan["replaces_revision_id"] = "synthetic-prior-amm-revision"
        assert(financialPlanAuthenticationReason(revisedAMMPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.") == "approve the revised plan “Synthetic bounded swap” (1 step, 10 minutes)")
        var spoofedAMMPlan = ammPlan; spoofedAMMPlan["name"] = "Different plan"
        assert(financialPlanAuthenticationReason(spoofedAMMPlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.") == nil)
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
        ramsesSpec["summary"] = "Swap up to 0.1 WETH for USDC; Ramses v3 tick spacing 10; quote fee ceiling 1000 ppm."
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
        assert(ramsesPlanReason == ammPlanReason)
        assert(financialPlanAuthenticationReason(relativePlan(ramsesPlan), summary:relativeSummary) == "approve the plan “Synthetic bounded swap” (1 step, 45 minutes)")
        for (operation, dex, _, tier) in [("sushi.swap", "sushi", "Sushi", 3000), ("pancakeswap.swap", "pancakeswap", "PancakeSwap", 2500)] {
            var step = swapPlanStep; step["operation"] = operation
            var input = swapInput; input["fee_tier"] = tier; step["input"] = input
            var fixed = ammSpec["fixed"] as! [String: Any]
            fixed["operation"] = operation; fixed["dex"] = dex
            fixed["factory"] = "0x4444444444444444444444444444444444444444"
            fixed["pool"] = "0x3333333333333333333333333333333333333333"
            fixed["pool_deployer"] = dex == "pancakeswap" ? "0x5555555555555555555555555555555555555555" as Any : NSNull()
            var parameters = ammSpec["parameters"] as! [String: Any]; parameters["allowed_fee_tiers"] = [tier]
            var spec = ammSpec; spec["fixed"] = fixed; spec["parameters"] = parameters
            var permissionBody = ammPlanPermission["permission"] as! [String: Any]; permissionBody["spec"] = spec
            var permission = ammPlanPermission; permission["permission"] = permissionBody
            var body = ammPlan["plan"] as! [String: Any]; body["steps"] = [step]
            var forkPlan = ammPlan; forkPlan["plan"] = body; forkPlan["permissions"] = [permission]
            let summary = "Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds."
            let reason = financialPlanAuthenticationReason(forkPlan, summary:summary)!
            assert(reason == ammPlanReason)
            assert(financialPlanAuthenticationReason(relativePlan(forkPlan), summary:relativeSummary) == nil)
            assert(reason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
            var revisedForkPlan = forkPlan; revisedForkPlan["replaces_revision_id"] = "synthetic-prior-fork-revision"
            let revisedReason = "approve the revised plan “Synthetic bounded swap” (1 step, 10 minutes)"
            assert(financialPlanAuthenticationReason(revisedForkPlan, summary:summary) == revisedReason)
            func reviewedPlan(_ scope: [String: Any], _ bounds: [String: Any], input selectedInput: [String: Any]) -> [String: Any] {
                var reviewedSpec = spec; reviewedSpec["fixed"] = scope; reviewedSpec["parameters"] = bounds
                var reviewedPermissionBody = permissionBody; reviewedPermissionBody["spec"] = reviewedSpec
                var reviewedPermission = permission; reviewedPermission["permission"] = reviewedPermissionBody
                var reviewedStep = step; reviewedStep["input"] = selectedInput
                var reviewedBody = body; reviewedBody["steps"] = [reviewedStep]
                var reviewed = forkPlan; reviewed["plan"] = reviewedBody; reviewed["permissions"] = [reviewedPermission]
                return reviewed
            }
            var legacyMultiple = parameters; legacyMultiple["allowed_fee_tiers"] = [500, tier]
            assert(financialPlanAuthenticationReason(reviewedPlan(fixed, legacyMultiple, input:input), summary:summary) == reason)
            var mappedFixed = fixed; mappedFixed.removeValue(forKey:"pool")
            mappedFixed["approved_pools"] = [String(tier):fixed["pool"]!]
            assert(financialPlanAuthenticationReason(reviewedPlan(mappedFixed, parameters, input:input), summary:summary) == reason)
            var revisedMappedPlan = reviewedPlan(mappedFixed, parameters, input:input)
            revisedMappedPlan["replaces_revision_id"] = "synthetic-prior-fork-revision"
            assert(financialPlanAuthenticationReason(revisedMappedPlan, summary:summary) == revisedReason)
            var allParameters = parameters; allParameters["allowed_fee_tiers"] = [100, 500, tier, 10000]
            var allFixed = mappedFixed
            allFixed["approved_pools"] = ["100":"0x1111111111111111111111111111111111111111", "500":"0x6666666666666666666666666666666666666666", String(tier):fixed["pool"]!, "10000":"0x7777777777777777777777777777777777777777"]
            var selected500 = input; selected500["fee_tier"] = 500
            assert(financialPlanAuthenticationReason(reviewedPlan(allFixed, allParameters, input:selected500), summary:summary) == reason)
            assert(financialPlanAuthenticationReason(reviewedPlan(mappedFixed, parameters, input:selected500), summary:summary) == nil)
            for (label, invalidFixed, invalidParameters) in invalidPoolMapScopes(fixed:mappedFixed, parameters:parameters, tier:tier, dex:dex) {
                assert(financialPlanAuthenticationReason(reviewedPlan(invalidFixed, invalidParameters, input:input), summary:summary) == nil, label)
                var invalidRevision = reviewedPlan(invalidFixed, invalidParameters, input:input)
                invalidRevision["replaces_revision_id"] = "synthetic-prior-fork-revision"
                assert(financialPlanAuthenticationReason(invalidRevision, summary:summary) == nil, label)
            }
            for field in ["dex", "factory", "pool", "router"] {
                var incomplete = fixed; incomplete.removeValue(forKey:field)
                var invalidSpec = spec; invalidSpec["fixed"] = incomplete
                var invalidBody = permissionBody; invalidBody["spec"] = invalidSpec
                var invalidPermission = permission; invalidPermission["permission"] = invalidBody
                forkPlan["permissions"] = [invalidPermission]
                assert(financialPlanAuthenticationReason(forkPlan, summary:summary) == nil, field)
            }
            var wrongFixed = fixed; wrongFixed["dex"] = "ramses"
            var invalidSpec = spec; invalidSpec["fixed"] = wrongFixed
            permissionBody["spec"] = invalidSpec; permission["permission"] = permissionBody; forkPlan["permissions"] = [permission]
            assert(financialPlanAuthenticationReason(forkPlan, summary:summary) == nil)
            permissionBody["spec"] = spec; permission["permission"] = permissionBody; forkPlan["permissions"] = [permission]
            for (field, value) in [("fee_tier", (dex == "sushi" ? 2500 : 3000) as Any), ("max_pool_fee_ppm", 1000 as Any), ("tick_spacing", 10 as Any), ("protocol", "v4" as Any)] {
                var invalidInput = input; invalidInput[field] = value
                var invalidStep = step; invalidStep["input"] = invalidInput
                body["steps"] = [invalidStep]; forkPlan["plan"] = body
                assert(financialPlanAuthenticationReason(forkPlan, summary:summary) == nil, field)
            }
            var secondStep = step; secondStep["step_id"] = "swap-2"
            var secondPermission = permission; secondPermission["step_id"] = "swap-2"
            body["steps"] = [step, secondStep]; forkPlan["plan"] = body; forkPlan["permissions"] = [permission, secondPermission]
            let multipleReason = financialPlanAuthenticationReason(forkPlan, summary:"Approve the plan “Synthetic bounded swap” with 2 ordered steps for 600 seconds.")!
            assert(multipleReason == "approve the plan “Synthetic bounded swap” (2 steps, 10 minutes)")
            var revisedMultiplePlan = forkPlan; revisedMultiplePlan["replaces_revision_id"] = "synthetic-prior-fork-revision"
            assert(financialPlanAuthenticationReason(revisedMultiplePlan, summary:"Approve the plan “Synthetic bounded swap” with 2 ordered steps for 600 seconds.") == "approve the revised plan “Synthetic bounded swap” (2 steps, 10 minutes)")
        }
        var higherAggregatePlan = ammPlan
        var higherAggregateBody = ammPlan["plan"] as! [String: Any]
        var higherAggregateBounds = higherAggregateBody["aggregate_bounds"] as! [String: Any]
        var higherCost = (higherAggregateBounds["maximum_costs"] as! [[String: Any]])[0]
        var higherCostAmount = higherCost["amount"] as! [String: Any]
        higherCostAmount["atoms"] = "2000000000000000"
        higherCost["amount"] = higherCostAmount
        higherAggregateBounds["maximum_costs"] = [higherCost]
        higherAggregateBody["aggregate_bounds"] = higherAggregateBounds
        higherAggregatePlan["plan"] = higherAggregateBody
        let higherAggregateReason = financialPlanAuthenticationReason(higherAggregatePlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.")!
        // Complete fee bindings stay intact even though the prompt identifies the plan.
        assert(higherAggregateReason == ammPlanReason)
        let boundCosts = ((higherAggregatePlan["plan"] as! [String: Any])["aggregate_bounds"] as! [String: Any])["maximum_costs"] as! [[String: Any]]
        assert((boundCosts[0]["amount"] as! [String: Any])["atoms"] as? String == "2000000000000000")
        var malformedAggregatePlan = higherAggregatePlan
        var malformedAggregateBody = higherAggregateBody
        higherCostAmount["atoms"] = "not-an-amount"
        higherCost["amount"] = higherCostAmount
        higherAggregateBounds["maximum_costs"] = [higherCost]
        malformedAggregateBody["aggregate_bounds"] = higherAggregateBounds
        malformedAggregatePlan["plan"] = malformedAggregateBody
        assert(financialPlanAuthenticationReason(malformedAggregatePlan, summary:"Approve the plan “Synthetic bounded swap” with 1 ordered steps for 600 seconds.") == nil)
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
        assert(twoSwapReason == "approve the plan “Synthetic bounded swap” (2 steps, 10 minutes)")
        var mixedStep = swapPlanStep
        mixedStep["step_id"] = "second-action"
        mixedStep["operation"] = "wallet.sweep"
        var mixedPlanBody = twoStepBody
        mixedPlanBody["steps"] = [swapPlanStep, mixedStep]
        var mixedPlan = twoSwapPlan
        mixedPlan["plan"] = mixedPlanBody
        let mixedExactPermission: [String: Any] = ["step_id":"second-action", "exact":[
            "scope":["mode":"exact", "maximum_debits":[[String:Any]](),
                     "maximum_costs":[[String:Any]](), "possible_credits":[[String:Any]]()],
            "definition_hash":"synthetic", "connections_hash":"synthetic"]]
        mixedPlan["permissions"] = [ammPlanPermission, mixedExactPermission]
        let mixedReason = financialPlanAuthenticationReason(mixedPlan, summary:"Approve the plan “Synthetic bounded swap” with 2 ordered steps for 600 seconds.")!
        assert(mixedReason == twoSwapReason)
        // Reproduce the five-step funding/withdrawal/bridge/swap shape in the report.
        var fundingSteps: [[String: Any]] = []
        var fundingPermissions: [[String: Any]] = []
        for (index, operation) in ["binance.spot.order-place", "binance.transfer", "binance.withdraw", "bridge.transfer"].enumerated() {
            let id = "funding-\(index)"
            var step = mixedStep; step["step_id"] = id; step["operation"] = operation
            var permission = mixedExactPermission; permission["step_id"] = id
            fundingSteps.append(step); fundingPermissions.append(permission)
        }
        fundingSteps.append(swapPlanStep); fundingPermissions.append(ammPlanPermission)
        let fundingName = "Fund Ethereum, bridge to Base and swap to USDC"
        var fundingPlan = ammPlan; fundingPlan["name"] = fundingName
        var fundingBody = mixedPlanBody; fundingBody["name"] = fundingName; fundingBody["steps"] = fundingSteps
        fundingPlan["plan"] = fundingBody; fundingPlan["permissions"] = fundingPermissions
        let fundingReason = financialPlanAuthenticationReason(fundingPlan, summary:"Approve the plan “\(fundingName)” with 5 ordered steps for 600 seconds.")!
        assert(fundingReason == "approve the plan “\(fundingName)” (5 steps, 10 minutes)")
        assert(fundingReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
        let coinbaseSource = "coinbase:portfolio:11111111-1111-4111-8111-111111111111"
        let coinbaseDestination = "0x1111111111111111111111111111111111111111"
        let coinbaseUsdc: [String: Any] = ["network":"coinbase", "reference":"USDC", "decimals":36]
        let coinbasePrincipalAtoms = "1" + String(repeating:"0", count:36)
        let coinbaseReserveAtoms = "1" + String(repeating:"0", count:35)
        let coinbaseScope: [String: Any] = ["mode":"exact",
            "maximum_debits":[["resource":coinbaseSource,
                "amount":["asset":coinbaseUsdc, "atoms":coinbasePrincipalAtoms]]],
            "maximum_costs":[["location":["kind":"cash", "resource":coinbaseSource],
                "amount":["asset":coinbaseUsdc, "atoms":coinbaseReserveAtoms],
                "treatment":"venue_calculated_additional_debit"]],
            "possible_credits":[["resource":"ethereum:8453:\(coinbaseDestination.lowercased())",
                "asset":["network":"ethereum:8453", "reference":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", "decimals":6]]]]
        let coinbaseWithdrawInput: [String: Any] = ["account":"SYNTHETIC-Coinbase", "accountId":"22222222-2222-4222-8222-222222222222",
            "destination":"SYNTHETIC-Base-wallet", "address":coinbaseDestination, "network":"base", "currency":"USDC",
            "amount":"1", "feeMode":"venue_calculated", "feeReserveUsdc":"0.1", "requestId":"withdraw-1"]
        let coinbaseWithdrawStep: [String: Any] = ["step_id":"withdraw", "operation":"coinbase.withdraw",
            "input":coinbaseWithdrawInput, "permitted_variations":coinbaseScope]
        let coinbaseWithdrawPermission: [String: Any] = ["step_id":"withdraw", "exact":["scope":coinbaseScope,
            "definition_hash":"SYNTHETIC-schema", "connections_hash":"SYNTHETIC-connections"]]
        let precedingStep: [String: Any] = ["step_id":"sell-hbar", "operation":"hedera.sell",
            "input":["amount":"2"], "permitted_variations":["mode":"exact", "maximum_debits":[], "maximum_costs":[], "possible_credits":[]]]
        let precedingPermission: [String: Any] = ["step_id":"sell-hbar", "exact":["scope":precedingStep["permitted_variations"]!,
            "definition_hash":"SYNTHETIC-hbar-schema", "connections_hash":"SYNTHETIC-hbar-connections"]]
        let coinbasePlanName = "Sell HBAR and withdraw proceeds to Base"
        let coinbasePlanSteps = [precedingStep, coinbaseWithdrawStep]
        let coinbaseAggregate: [String: Any] = ["maximum_debits":coinbaseScope["maximum_debits"]!,
            "maximum_costs":coinbaseScope["maximum_costs"]!]
        let coinbasePlan: [String: Any] = ["version":1, "kind":"financial_plan", "name":coinbasePlanName,
            "plan_id":"SYNTHETIC-coinbase-plan", "revision_id":"SYNTHETIC-coinbase-revision",
            "plan":["name":coinbasePlanName, "steps":coinbasePlanSteps, "aggregate_bounds":coinbaseAggregate],
            "permissions":[precedingPermission, coinbaseWithdrawPermission], "valid_for_seconds":600]
        let coinbasePlanSummary = "Approve the plan “\(coinbasePlanName)” with 2 ordered steps for 600 seconds."
        let coinbasePlanReason = financialPlanAuthenticationReason(coinbasePlan, summary:coinbasePlanSummary)!
        assert(coinbasePlanReason.contains("Sell HBAR and withdraw proceeds to Base"))
        assert(coinbasePlanReason.contains("Coinbase-calculated fees are not capped"))
        assert(coinbasePlanReason.contains("reserve is accounting only"))
        assert(coinbasePlanReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
        var principalOnlyWithdrawStep = coinbaseWithdrawStep
        principalOnlyWithdrawStep["funding_dependencies"] = [["producing_step_id":"sell-hbar", "funding_scope":"principal_only"]]
        var principalOnlyPlan = coinbasePlan
        principalOnlyPlan["plan"] = ["name":coinbasePlanName, "steps":[precedingStep, principalOnlyWithdrawStep], "aggregate_bounds":coinbaseAggregate]
        assert(financialPlanAuthenticationReason(principalOnlyPlan, summary:coinbasePlanSummary) == coinbasePlanReason)
        var unsafeFundingStep = coinbaseWithdrawStep
        unsafeFundingStep["funding_dependencies"] = [["producing_step_id":"sell-hbar"]]
        var unsafeFundingPlan = coinbasePlan
        unsafeFundingPlan["plan"] = ["name":coinbasePlanName, "steps":[precedingStep, unsafeFundingStep], "aggregate_bounds":coinbaseAggregate]
        assert(financialPlanAuthenticationReason(unsafeFundingPlan, summary:coinbasePlanSummary) == nil)
        func coinbasePlanWith(input: [String: Any] = coinbaseWithdrawInput,
                              permission: [String: Any] = coinbaseWithdrawPermission,
                              aggregate: [String: Any] = coinbaseAggregate,
                              stepScope: [String: Any] = coinbaseScope) -> [String: Any] {
            var changedStep = coinbaseWithdrawStep; changedStep["input"] = input; changedStep["permitted_variations"] = stepScope
            var changedPlan = coinbasePlan
            changedPlan["plan"] = ["name":coinbasePlanName, "steps":[precedingStep, changedStep], "aggregate_bounds":aggregate]
            changedPlan["permissions"] = [precedingPermission, permission]
            return changedPlan
        }
        for (field, invalid) in [("feeMode", "capped" as Any), ("network", "ethereum" as Any),
            ("feeReserveUsdc", "0" as Any), ("feeReserveUsdc", "0.0000001" as Any),
            ("amount", "1.0000001")] {
            var badInput = coinbaseWithdrawInput; badInput[field] = invalid
            assert(financialPlanAuthenticationReason(coinbasePlanWith(input:badInput), summary:coinbasePlanSummary) == nil, "invalid Coinbase withdrawal \(field)")
        }
        var changedScope = coinbaseScope; changedScope["maximum_costs"] = []
        var changedStepScope = coinbaseScope; changedStepScope["maximum_costs"] = []
        assert(financialPlanAuthenticationReason(coinbasePlanWith(permission:["step_id":"withdraw", "exact":["scope":changedScope,
            "definition_hash":"SYNTHETIC-schema", "connections_hash":"SYNTHETIC-connections"]], stepScope:changedStepScope), summary:coinbasePlanSummary) == nil)
        for field in ["definition_hash", "connections_hash"] {
            var badPermission = coinbaseWithdrawPermission; var exact = badPermission["exact"] as! [String: Any]
            exact.removeValue(forKey:field); badPermission["exact"] = exact
            assert(financialPlanAuthenticationReason(coinbasePlanWith(permission:badPermission), summary:coinbasePlanSummary) == nil, "missing \(field)")
        }
        for mismatch in ["treatment", "resource", "asset", "atoms"] {
            var badScope = coinbaseScope
            var cost = (badScope["maximum_costs"] as! [[String: Any]])[0]
            if mismatch == "treatment" { cost["treatment"] = "additional_debit" }
            if mismatch == "resource" { cost["location"] = ["kind":"cash", "resource":"coinbase:portfolio:other"] }
            if mismatch == "asset" { cost["amount"] = ["asset":["network":"ethereum:8453", "reference":"USDC", "decimals":6], "atoms":coinbaseReserveAtoms] }
            if mismatch == "atoms" { cost["amount"] = ["asset":coinbaseUsdc, "atoms":"1"] }
            badScope["maximum_costs"] = [cost]
            let badStepScope = badScope
            assert(financialPlanAuthenticationReason(coinbasePlanWith(permission:["step_id":"withdraw", "exact":["scope":badScope,
                "definition_hash":"SYNTHETIC-schema", "connections_hash":"SYNTHETIC-connections"]], stepScope:badStepScope), summary:coinbasePlanSummary) == nil, "bad fee \(mismatch)")
        }
        var insufficientAggregate = coinbaseAggregate
        insufficientAggregate["maximum_costs"] = [["location":["kind":"cash", "resource":coinbaseSource],
            "amount":["asset":coinbaseUsdc, "atoms":"1"], "treatment":"venue_calculated_additional_debit"]]
        assert(financialPlanAuthenticationReason(coinbasePlanWith(aggregate:insufficientAggregate), summary:coinbasePlanSummary) == nil)
        var longCoinbasePlan = coinbasePlan
        let longCoinbaseName = String(repeating:"N", count:78)
        longCoinbasePlan["name"] = longCoinbaseName
        longCoinbasePlan["plan"] = ["name":longCoinbaseName, "summary":String(repeating:"s", count:60),
            "steps":coinbasePlanSteps, "aggregate_bounds":coinbaseAggregate]
        let longCoinbaseSummary = "Approve the plan “\(longCoinbaseName)” with 2 ordered steps for 600 seconds."
        let longCoinbasePlanReason = financialPlanAuthenticationReason(longCoinbasePlan, summary:longCoinbaseSummary)!
        assert(longCoinbasePlanReason.contains(longCoinbaseName))
        assert(longCoinbasePlanReason.contains("Coinbase-calculated fees are not capped"))
        assert(!longCoinbasePlanReason.contains(String(repeating:"s", count:60)))
        assert(longCoinbasePlanReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
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
        for index in 1...12 {
            var step = swapPlanStep; step["step_id"] = "swap-\(index)"; longSteps.append(step)
            var permission = ammPlanPermission; permission["step_id"] = "swap-\(index)"; longPermissions.append(permission)
        }
        var longPlanBody = ammPlan["plan"] as! [String: Any]; longPlanBody["steps"] = longSteps
        var longAggregateBounds = longPlanBody["aggregate_bounds"] as! [String: Any]
        var longDebit = (longAggregateBounds["maximum_debits"] as! [[String: Any]])[0]
        var longDebitAmount = longDebit["amount"] as! [String: Any]
        longDebitAmount["atoms"] = "1200000000000000000"
        longDebit["amount"] = longDebitAmount
        longAggregateBounds["maximum_debits"] = [longDebit]
        var longCost = (longAggregateBounds["maximum_costs"] as! [[String: Any]])[0]
        var longCostAmount = longCost["amount"] as! [String: Any]
        longCostAmount["atoms"] = "12000000000000000"
        longCost["amount"] = longCostAmount
        longAggregateBounds["maximum_costs"] = [longCost]
        longPlanBody["aggregate_bounds"] = longAggregateBounds
        longAMMPlan["plan"] = longPlanBody; longAMMPlan["permissions"] = longPermissions
        let longAMMReason = financialPlanAuthenticationReason(longAMMPlan, summary:"Approve the plan “Synthetic bounded swap” with 12 ordered steps for 600 seconds.")!
        assert(longAMMReason == "approve the plan “Synthetic bounded swap” (12 steps, 10 minutes)")
        assert(longAMMReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
        var longNamedAMMPlan = ammPlan
        longNamedAMMPlan["name"] = longPlanName
        var longNamedAMMBody = ammPlan["plan"] as! [String: Any]
        longNamedAMMBody["name"] = longPlanName
        longNamedAMMPlan["plan"] = longNamedAMMBody
        assert(financialPlanAuthenticationReason(longNamedAMMPlan, summary:"Approve the plan “\(longPlanName)” with 1 ordered steps for 600 seconds.") == "approve the plan “\(longPlanName)” (1 step, 10 minutes)")
        assert(financialPlanAuthenticationReason(ammPlan, summary:"Approve a different plan") == nil)
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
