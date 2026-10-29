import Foundation

@main
struct ApprovalPresentationTests {
    static func main() {
        // Cross-language contract tests can feed an actual canonical binding to
        // the same renderer without starting LocalAuthentication.
        if CommandLine.arguments.dropFirst().first == "--render-binding" {
            let data = FileHandle.standardInput.readDataToEndOfFile()
            guard let binding = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { exit(2) }
            let response: [String: String]
            switch typedFinancialApprovalReason(binding) {
            case .success(let reason): response = ["status":"ok", "reason":reason]
            case .failure(let failure): response = ["status":failure.rawValue]
            }
            guard let encoded = try? JSONSerialization.data(withJSONObject: response, options: [.sortedKeys]),
                  let text = String(data: encoded, encoding: .utf8) else { exit(2) }
            print(text)
            return
        }
        // Typed reviews retain every field and count extended grapheme clusters.
        func envelope(_ review: [String: Any]) -> [String: Any] {
            ["version":1, "kind":"financial_approval_review", "purpose":(review["action"] as? String == "execute-plan" ? "financial_plan" : "financial_write"), "execution_binding":[:], "review":review]
        }
        let asset: [String: Any] = ["name":"USDC", "identifier":["network":"ethereum:1", "reference":"native", "decimals":6]]
        let leg: [String: Any] = ["amount":"1.25", "asset":asset, "account":["name":"wallet", "domain":"chain"]]
        let partialTransfer: [String: Any] = ["version":2, "action":"account-transfer", "service":"Example",
            "input":["asset":asset, "account":["name":"source"]],
            "output":["amount":"5", "asset":asset, "account":["name":"destination"]]]
        guard case .success(let transferReason) = typedFinancialApprovalReason(envelope(partialTransfer)) else { fatalError("Valid partial transfer rejected") }
        assert(transferReason == "transfer 5 USDC from source to destination on Example; at least 5 USDC; input: USDC", transferReason)
        // The fixture enumerates Rust’s generated ApprovalAction schema and
        // serializes each validated review through the native contract.
        if let path = ProcessInfo.processInfo.environment["GOFIN_NATIVE_FINANCIAL_FIXTURES"] {
            let data = try! Data(contentsOf: URL(fileURLWithPath: path))
            let bindings = try! JSONSerialization.jsonObject(with: data) as! [[String: Any]]
            assert(!bindings.isEmpty)
            for binding in bindings {
                let action = (binding["review"] as! [String: Any])["action"] as! String
                guard case .success(let reason) = typedFinancialApprovalReason(binding) else { fatalError("Rust-validated review rejected: \(action)") }
                assert(reason.contains("Example") && reason.contains("fee cap 0.01 USDC"), action)
                assert(reason.contains("ORE") && reason.contains("USDC") && !reason.contains("synthetic-ore-address"), action + ": " + reason)
            }
        }
        var identityReview: [String: Any] = ["version":2, "action":"claim", "service":"Example", "input":["asset":["name":"ORE", "identifier":["network":"solana:mainnet", "reference":"synthetic-ore-address", "decimals":11]]]]
        guard case .success(let oreReason) = typedFinancialApprovalReason(envelope(identityReview)), oreReason.contains("ORE"), !oreReason.contains("synthetic-ore-address") else { fatalError("asset name") }
        for invalid: Any in ["ORE", ["name":"ORE"], ["name":"ORE", "identifier":["network":"", "reference":"mint", "decimals":11]], ["name":"ORE", "identifier":["network":"solana", "reference":"mint", "decimals":256]]] {
            identityReview["input"] = ["asset":invalid]
            guard case .failure(.invalid) = typedFinancialApprovalReason(envelope(identityReview)) else { fatalError("invalid identity") }
        }
        let longAmount = String(repeating: "9", count: 101)
        let sparse: [String: Any] = ["version":2, "action":"claim", "service":"Example", "input":[:], "output":["amount":longAmount]]
        guard case .success(let sparseReason) = typedFinancialApprovalReason(envelope(sparse)) else { fatalError("Rust-valid sparse review rejected") }
        assert(sparseReason.contains(longAmount))
        let actions = ["swap", "account-transfer", "exchange-withdrawal", "exchange-deposit", "cancel-order", "lending-borrow", "lending-deposit", "vault-deposit", "set-leverage", "lending-repay", "collect-fees", "bridge"]
        for action in actions {
            let review: [String: Any] = ["version":2, "action":action, "service":"Example", "market":"market", "input":leg, "output":leg, "other_details":"fee cap 0.01 USDC"]
            guard case .success(let reason) = typedFinancialApprovalReason(envelope(review)) else { fatalError(action) }
            let expected: [String: String] = [
                "swap":"swap 1.25 USDC for at least 1.25 USDC on Example in market via wallet (chain)",
                "account-transfer":"transfer 1.25 USDC from wallet (chain) to wallet (chain) on Example in market; at least 1.25 USDC",
                "exchange-withdrawal":"withdraw 1.25 USDC from wallet (chain) to wallet (chain) on Example in market; at least 1.25 USDC",
                "exchange-deposit":"deposit 1.25 USDC from wallet (chain) to wallet (chain) on Example in market; at least 1.25 USDC",
                "cancel-order":"cancel order 1.25 USDC on Example in market from wallet (chain)",
                "lending-borrow":"borrow 1.25 USDC on Example in market to wallet (chain)",
                "lending-deposit":"supply 1.25 USDC on Example in market from wallet (chain)",
                "vault-deposit":"deposit into vault 1.25 USDC on Example in market from wallet (chain)",
                "set-leverage":"set leverage 1.25 USDC on Example in market from wallet (chain)",
                "lending-repay":"repay 1.25 USDC on Example in market from wallet (chain)",
                "collect-fees":"collect fees 1.25 USDC on Example in market from wallet (chain)",
                "bridge":"bridge 1.25 USDC from wallet (chain) to wallet (chain) on Example in market; at least 1.25 USDC"
            ]
            assert(reason == expected[action]! + "; fee cap 0.01 USDC", action + ": " + reason)
        }
        var review: [String: Any] = ["version":2, "action":"execute-plan", "service":"GoFin", "description":"rebalance", "step_count":2, "valid_for_seconds":60]
        guard case .success(let plan) = typedFinancialApprovalReason(envelope(review)) else { fatalError("plan") }
        assert(plan == "approve 2 steps for 1 min: rebalance on GoFin")
        for action in ["swap", "account-transfer", "bridge", "exchange-withdrawal", "exchange-deposit"] {
            var missing: [String: Any] = ["version":2, "action":action, "service":"Example", "input":leg]
            guard case .failure(.invalid) = typedFinancialApprovalReason(envelope(missing)) else { fatalError("missing output: " + action) }
            missing["output"] = ["amount":"1", "asset":asset]
            if action != "swap" {
                guard case .failure(.invalid) = typedFinancialApprovalReason(envelope(missing)) else { fatalError("missing destination account: " + action) }
            }
        }
        let boundaryReview: [String: Any] = ["version":2, "action":"authorize-boundaries", "service":"Example", "market":"ETH/USDC", "input":leg, "other_details":"valid for 600 seconds"]
        var boundaryEnvelope = envelope(boundaryReview); boundaryEnvelope["purpose"] = "financial_boundary"
        guard case .success(let boundary) = typedFinancialApprovalReason(boundaryEnvelope) else { fatalError("boundary") }
        assert(boundary == "authorize ETH/USDC on Example with up to 1.25 USDC via wallet (chain); valid for 600 seconds")
        review = ["version":2, "action":"collect-fees", "service":"x", "other_details":String(repeating:"👩‍💻", count:200)]
        guard case .success = typedFinancialApprovalReason(envelope(review)) else { fatalError("200 graphemes") }
        review["other_details"] = String(repeating:"👩‍💻", count:201)
        guard case .failure(.tooLong) = typedFinancialApprovalReason(envelope(review)) else { fatalError("201 graphemes") }
        review["other_details"] = String(repeating:"e\u{301}", count:200)
        guard case .success = typedFinancialApprovalReason(envelope(review)) else { fatalError("combining") }
        review["other_details"] = "bad\u{202e}text"
        guard case .failure = typedFinancialApprovalReason(envelope(review)) else { fatalError("bidi") }
        review = ["version":2, "action":"collect-fees", "service":String(repeating:"x", count:224)]
        guard case .success(let exact) = typedFinancialApprovalReason(envelope(review)), exact.count == 240 else { fatalError("240") }
        review["service"] = String(repeating:"x", count:225)
        guard case .failure(.tooLong) = typedFinancialApprovalReason(envelope(review)) else { fatalError("241") }
        review["action"] = "unknown"
        guard case .failure(.invalid) = typedFinancialApprovalReason(envelope(review)) else { fatalError("unknown") }
        guard case .failure(.invalid) = typedFinancialApprovalReason([:]) else { fatalError("missing review") }
        let validReview: [String: Any] = ["version":2, "action":"collect-fees", "service":"Example"]
        for (field, invalid) in [("version", 2 as Any), ("kind", "unknown" as Any), ("purpose", "configuration" as Any), ("execution_binding", NSNull() as Any)] {
            var badEnvelope = envelope(validReview); badEnvelope[field] = invalid
            guard case .failure(.invalid) = typedFinancialApprovalReason(badEnvelope) else { fatalError("bad envelope " + field) }
        }
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
        let overrideOperationID = "11111111-2222-4333-8444-555555555555"
        let overrideReview: [String: Any] = [
            "version":1, "operation_id": overrideOperationID, "operation":"kraken.add-order",
            "resources":["kraken:synthetic:spot"],
            "record_hash":String(repeating:"a", count:64),
            "reservations_hash":String(repeating:"b", count:64),
            "submission":["kind":"synthetic","reference":"original-request","data":[:]],
            "released_reservations":[
                ["resource":"lock:kraken:synthetic:spot","units":"0","fee_units":"0"],
                ["resource":"cash:synthetic","units":"450000000","fee_units":"1000000"]
            ],
            "released_cash":[["resource":"kraken:synthetic:spot",
                "asset":["network":"kraken","reference":"SOL","decimals":9],
                "amount_atoms":"451000000"]],
            "remaining_reservations":[],
            "warning":"The original action may still execute."
        ]
        let overrideBinding: [String: Any] = [
            "action":"reservation_override","version":1,"request_id":"synthetic-override",
            "review_hash":String(repeating:"c", count:64),"review":overrideReview
        ]
        let overrideReason = reservationOverrideAuthenticationReason(overrideBinding)!
        assert(overrideReason.contains(overrideOperationID))
        assert(overrideReason.contains("kraken.add-order"))
        assert(overrideReason.contains("The original action may still execute"))
        assert(overrideReason.contains("0.451 SOL@kraken (incl. fees)"))
        assert(overrideReason.contains("risking double use"))
        assert(overrideReason.contains("This does not cancel it"))
        assert(overrideReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
        for (key, value) in [
            ("operation_id", "spoof\nanother operation" as Any),
            ("operation_id", "spoof\u{202e}operation" as Any),
            ("operation_id", String(repeating:"a", count:128) as Any),
            ("resources", [] as Any),
            ("resources", ["same","same"] as Any),
            ("record_hash", "invalid" as Any),
            ("reservations_hash", "invalid" as Any),
            ("warning", "" as Any),
            ("remaining_reservations", "invalid" as Any),
            ("released_reservations", [] as Any),
            ("released_reservations", [["resource":"cash","units":"-1","fee_units":"0"]] as Any),
            ("released_reservations", [["resource":"cash","units":1,"fee_units":"0"]] as Any),
            ("released_cash", [] as Any),
            ("released_cash", [["resource":"kraken:synthetic:spot","asset":["network":"kraken","reference":"SOL","decimals":9],"amount_atoms":"0"]] as Any),
            ("released_cash", [["resource":"other-account","asset":["network":"kraken","reference":"SOL","decimals":9],"amount_atoms":"1"]] as Any),
            ("submission", "invalid" as Any),
            ("version", true as Any),
            ("version", 2 as Any)
        ] {
            var invalidReview = overrideReview
            invalidReview[key] = value
            var invalid = overrideBinding
            invalid["review"] = invalidReview
            assert(reservationOverrideAuthenticationReason(invalid) == nil, "override rejects \(key)")
        }
        for (key, value) in [
            ("action","other" as Any), ("version",true as Any),
            ("review_hash","invalid" as Any), ("review",[:] as Any)
        ] {
            var invalid = overrideBinding
            invalid[key] = value
            assert(reservationOverrideAuthenticationReason(invalid) == nil)
        }
        var legacyUnknownReview = overrideReview
        legacyUnknownReview["submission"] = NSNull()
        var legacyUnknownBinding = overrideBinding
        legacyUnknownBinding["review"] = legacyUnknownReview
        assert(reservationOverrideAuthenticationReason(legacyUnknownBinding) == nil)
        var locksOnlyReview = overrideReview
        locksOnlyReview["released_reservations"] = [["resource":"lock:kraken:synthetic:spot","units":"0","fee_units":"0"]]
        locksOnlyReview["released_cash"] = []
        var locksOnlyBinding = overrideBinding
        locksOnlyBinding["review"] = locksOnlyReview
        assert(!reservationOverrideAuthenticationReason(locksOnlyBinding)!.contains("incl. fees"))
        var twoAssetsReview = overrideReview
        twoAssetsReview["released_reservations"] = (overrideReview["released_reservations"] as! [[String: Any]]) +
            [["resource":"cash:synthetic-eth","units":"0","fee_units":"15000000000000"]]
        twoAssetsReview["released_cash"] = (overrideReview["released_cash"] as! [[String: Any]]) +
            [["resource":"kraken:synthetic:spot","asset":["network":"kraken","reference":"XETH","decimals":18],"amount_atoms":"15000000000000"]]
        var twoAssetsBinding = overrideBinding
        twoAssetsBinding["review"] = twoAssetsReview
        let twoAssetsReason = reservationOverrideAuthenticationReason(twoAssetsBinding)!
        assert(twoAssetsReason.contains("0.451 SOL@kraken, 0.000015 XETH@kraken"))
        assert(twoAssetsReason.contains(overrideOperationID))
        assert(twoAssetsReason.contains("risking double use"))
        assert(twoAssetsReason.unicodeScalars.count <= authenticationReasonMaximumCharacters)
        var oversizedCash = twoAssetsReview["released_cash"] as! [[String: Any]]
        oversizedCash[0]["amount_atoms"] = String(repeating:"9", count:100)
        twoAssetsReview["released_cash"] = oversizedCash
        twoAssetsBinding["review"] = twoAssetsReview
        assert(reservationOverrideAuthenticationReason(twoAssetsBinding) == nil, "material amounts must never be truncated")
        let resolutionAsset:[String:Any] = ["network":"kraken","reference":"USDC","decimals":6]
        func reportedAmount(_ atoms:String) -> [String:Any] { ["asset":resolutionAsset,"atoms":atoms] }
        let resolutionReview:[String:Any] = [
            "version":1,"operation_id":overrideOperationID,"operation":"kraken.create-funding-withdrawal-v1",
            "original_request_id":"funding-500","fingerprint":String(repeating:"f",count:64),
            "record_hash":String(repeating:"a",count:64),"reservations_hash":String(repeating:"b",count:64),"plan_hash":String(repeating:"c",count:64),
            "submission":["kind":"kraken","reference":"original-submission","data":[:]],
            "resources":["kraken:synthetic:spot"],
            "released_reservations":[["resource":"cash:synthetic","units":"500627000","fee_units":"0"]],
            "released_cash":[["resource":"kraken:synthetic:spot","asset":resolutionAsset,"amount_atoms":"500627000"]],
            "remaining_reservations":[],"impact":["historical_cap_breach":false,"future_authority_increased":false,"allocation_charges_retained":true,"plan_usage_retained":true,"receipt_provenance":"human_attested","plan_linked":true],"warnings":["Human-attested, not externally verified."],
            "attestation":["outcome":"succeeded","reason":"Destination credited",
                "actual_debits":[["resource":"kraken:synthetic:spot","amount":reportedAmount("500626000")]],
                "actual_costs":[["location":["kind":"cash","resource":"kraken:synthetic:spot"],"amount":reportedAmount("626000"),"treatment":"included_in_debit"]],
                "reported_receipts":[["resource":"coinbase:synthetic:spot","amount":reportedAmount("500000000")]]]
        ]
        let resolutionBinding:[String:Any] = ["version":1,"action":"operation_resolution","request_id":"resolve-500",
            "review_hash":String(repeating:"d",count:64),"review":resolutionReview]
        let resolutionReason = operationResolutionAuthenticationReason(resolutionBinding)!
        for text in [overrideOperationID,"succeeded","500.626 USDC@kraken","0.626 USDC@kraken","500 USDC@kraken","500.627 USDC@kraken","included in debit","Human-attested, not externally verified","Original may still act; no resubmission"] {
            assert(resolutionReason.contains(text), text)
        }
        assert(resolutionReason.unicodeScalars.count <= operationResolutionReasonMaximumCharacters)
        for key in ["submission","record_hash","reservations_hash","plan_hash","attestation","released_cash","remaining_reservations","impact","warnings"] {
            var review = resolutionReview; review.removeValue(forKey:key)
            var binding = resolutionBinding; binding["review"] = review
            assert(operationResolutionAuthenticationReason(binding) == nil, key)
        }
        for (key,value) in [("outcome","pending" as Any),("reason","fake\nreview" as Any),("actual_debits",[["resource":"account","amount":["asset":resolutionAsset,"atoms":"-1"]]] as Any),("actual_costs",[["amount":reportedAmount("626000")]] as Any)] {
            var review = resolutionReview; var attestation = review["attestation"] as! [String:Any]
            attestation[key] = value; review["attestation"] = attestation
            var binding = resolutionBinding; binding["review"] = review
            assert(operationResolutionAuthenticationReason(binding) == nil, key)
        }
        var standaloneReview = resolutionReview; standaloneReview["plan_hash"] = NSNull()
        var standaloneImpact = standaloneReview["impact"] as! [String:Any]; standaloneImpact["plan_linked"] = false
        standaloneReview["impact"] = standaloneImpact
        var standaloneBinding = resolutionBinding; standaloneBinding["review"] = standaloneReview
        assert(operationResolutionAuthenticationReason(standaloneBinding) != nil)
        var failedReview = resolutionReview; var failedAttestation = failedReview["attestation"] as! [String:Any]
        failedAttestation["outcome"] = "failed"; failedReview["attestation"] = failedAttestation
        var failedBinding = resolutionBinding; failedBinding["review"] = failedReview
        assert(operationResolutionAuthenticationReason(failedBinding)!.contains("failed."))
        var retainedReview = resolutionReview
        retainedReview["released_cash"] = []; retainedReview["released_reservations"] = []
        retainedReview["remaining_reservations"] = [["resource":"risk:derivative","units":"100","fee_units":"2"]]
        var retainedBinding = resolutionBinding; retainedBinding["review"] = retainedReview
        let retainedReason = operationResolutionAuthenticationReason(retainedBinding)!
        assert(retainedReason.contains("1 holds remain; their resources stay blocked"))
        assert(retainedReason.contains("release holds: none"))
        var largeReview = resolutionReview; var largeAttestation = largeReview["attestation"] as! [String:Any]
        largeAttestation["reported_receipts"] = Array(repeating:["resource":"coinbase:synthetic:spot","amount":reportedAmount(String(repeating:"9",count:100))],count:8)
        largeReview["attestation"] = largeAttestation
        var largeBinding = resolutionBinding; largeBinding["review"] = largeReview
        let largeReason = operationResolutionAuthenticationReason(largeBinding)!
        assert(largeReason.unicodeScalars.count > operationResolutionReasonMaximumCharacters, "native helper must reject overflow before asking for approval")
        assert(largeReason.contains(String(repeating:"9",count:94)), "exact amounts are never truncated")
        // This file is regenerated from real Rust types by `make test-ui`.
        // JSONSerialization is essential: hand-built Swift dictionaries missed
        // the engine's JSON tuple reservation keys in the original regression.
        if let fixturePath = ProcessInfo.processInfo.environment["GOFIN_NATIVE_RESOLUTION_FIXTURE"] {
            let data = try! Data(contentsOf:URL(fileURLWithPath:fixturePath))
            let fixture = try! JSONSerialization.jsonObject(with:data) as! [String:Any]
            let binding = fixture["binding"] as! [String:Any]
            let review = binding["review"] as! [String:Any]
            let rows = review["released_reservations"] as! [[String:Any]]
            let actualTypedKey = rows[0]["resource"] as! String
            assert(actualTypedKey.hasPrefix("["))
            assert(actualTypedKey.range(of:"^[A-Za-z0-9._:-]+$",options:.regularExpression) == nil,
                   "the previous simple-ID guard rejected this genuine Rust cash key")
            let reason = try! operationResolutionApprovalReason(binding).get()
            assert(reason.unicodeScalars.count <= operationResolutionReasonMaximumCharacters)
            for field in ["expected_quantities","expected_resources","expected_asset_references"] {
                for exact in fixture[field] as! [String] { assert(reason.contains(exact), "exact \(field) must remain visible: \(exact)") }
            }
            assert(reason.contains(review["operation_id"] as! String))
            assert(reason.contains("Human-attested, not externally verified"))
            assert(reason.contains("no resubmission"))
            assert(reason.contains("their resources stay blocked"))
            let malformedKeys:[Any] = [
                NSNull(), "[invalid]", " \(actualTypedKey)", "[null,null]",
                actualTypedKey.replacingOccurrences(of:"\"decimals\":6",with:"\"decimals\":true"),
                actualTypedKey.replacingOccurrences(of:"\"decimals\":6",with:"\"decimals\":6,\"unknown\":1"),
                actualTypedKey.replacingOccurrences(of:"\"decimals\":6",with:"\"decimals\":18,\"decimals\":6"),
                actualTypedKey.replacingOccurrences(of:"ethereum:1",with:"ethereum\\n:1")
            ]
            for key in malformedKeys {
                var invalidRows = rows; invalidRows[0]["resource"] = key
                var invalidReview = review; invalidReview["released_reservations"] = invalidRows
                var invalidBinding = binding; invalidBinding["review"] = invalidReview
                switch operationResolutionApprovalReason(invalidBinding) {
                case .success: assertionFailure("Malformed typed reservation key must fail closed")
                case .failure(let error): assert(error.rawValue == "invalid_resolution_review")
                }
            }
            var positionRows = rows; positionRows[0]["resource"] = fixture["position_cost_key"]
            var positionReview = review; positionReview["released_reservations"] = positionRows
            var positionBinding = binding; positionBinding["review"] = positionReview
            assert(operationResolutionAuthenticationReason(positionBinding) != nil,
                   "Rust position-cost tuple serialization must also be recognized")
            var oversizedReview = review; var oversizedAttestation = review["attestation"] as! [String:Any]
            oversizedAttestation["reason"] = String(repeating:"r",count:1024)
            oversizedReview["attestation"] = oversizedAttestation
            var oversizedBinding = binding; oversizedBinding["review"] = oversizedReview
            switch operationResolutionApprovalReason(oversizedBinding) {
            case .success: assertionFailure("A long review must never be silently truncated")
            case .failure(let error): assert(error.rawValue == "review_too_large")
            }
        }
        print("Approval presentation, terminal-proof/channel and authentication-reason cases passed; no UI or authentication requested.")
    }
}
