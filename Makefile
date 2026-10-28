.DEFAULT_GOAL := help
SHELL := /bin/sh
ARGS ?= --help
DEV_ENV ?= local
DEV_MODE ?= synthetic
AAVE_FORK_CHAIN ?= 1
AAVE_FORK_PIN ?=
AAVE_FORK_POOL ?=
DEV_CREDENTIALS_FILE ?=
DEV_IMPORT_FLAGS ?=
DEV_FLAGS ?=
AMM_FORK_ARGS ?=

.PHONY: help dev build dev-env-start dev-env-import dev-env-cli dev-env-stop dev-env-reset catalog test-ui test-native-approval test-dev-env test-coinbase test-solana-cex-funding test-binance-futures-rehearsal test-kamino-rehearsal test-kamino-validator test-meteora-rehearsal test-meteora-validator test-meteora-cli-validator test-aave-mcp-read-cli test-papertrade-rehearsal test-fork test-amm-fork test-aave-fork test-integration test-delivery test check bundle release setup update uninstall

help:
	@printf '%s\n' \
	  'make dev [ARGS="..."]  Run the debug CLI; broker commands use the installed service' \
	  'make dev-env-start [DEV_ENV=name] [DEV_MODE=synthetic|live] [DEV_FIXTURE=solana]  Build and start this worktree environment' \
	  'make dev-env-import [DEV_ENV=name] DEV_CREDENTIALS_FILE=/path/.env  Import development credentials while stopped' \
	  'make dev-env-cli [DEV_ENV=name] ARGS="..."  Invoke that environment worktree CLI' \
	  'make dev-env-stop [DEV_ENV=name]   Stop only that environment; preserve its journal' \
	  'make dev-env-reset [DEV_ENV=name]  Explicitly remove only that stopped environment state' \
	  '  DEV_ENV defaults to local, DEV_MODE defaults to synthetic; DEV_FLAGS=--json gives lifecycle JSON' \
	  'make build             Incrementally build the debug CLI and broker' \
	  'make catalog           Regenerate offline financial discovery from Rust definitions' \
	  'make test-e2e          Required positive CLI scenarios; Anvil, Surfpool or mocked backends' \
	  'make test-e2e-list     List executable scenarios and required coverage gaps' \
	  'make test-security     Explicit critical negative regressions; SECURITY_ARGS=--list' \
	  'make test-cli-regressions Deliberately break CLI discovery and execution, restore and recheck' \
	  'make test-ui           CLI discovery scenario and critical CLI/presentation negatives' \
	  'make test-native-approval  Rust-to-Swift approval contracts and native presentation; no Touch ID prompts' \
	  'make test-dev-env      Synthetic environment, recovery and fork-runner tests; no Anvil' \
	  'make test-coinbase     Coinbase contracts, signing and broker workflow fixtures; no live writes' \
	  'make test-solana-cex-funding  Public CLI Binance/Kraken Solana funding rehearsals; synthetic funds' \
	  'make test-binance-futures-rehearsal Public CLI leveraged futures funding/trade/recovery; synthetic funds' \
	  'make test-kamino-rehearsal Public CLI Jupiter/Kamino plan and recovery workflows; synthetic transports' \
	  'make test-kamino-validator Native Kamino lending/liquidity against owned cloned programs; requires ignored .context fixtures' \
	  'make test-kamino-borrowing-validator Public CLI collateral/borrow/repay/recovery on owned cloned programs' \
	  'make test-meteora-rehearsal Public GoFin CLI Meteora swap/plan/recovery rehearsal; synthetic funds' \
	  'make test-aave-mcp-read-cli Aave MCP catalog and position reads through the public CLI; synthetic fixture, no RPC or credentials' \
	  'make test-papertrade-rehearsal Public CLI Papertrade accounting and Core funding recovery rehearsal; synthetic evidence' \
	  'make test-full-plan-rehearsal Public CLI Kraken/Aave/bridge/Hyperliquid single-plan and recovery rehearsal; synthetic evidence' \
	  'make test-meteora-validator GoFin Meteora adapter tests on cloned local programs; requires ignored .context fixtures and solana-test-validator' \
	  'make test-meteora-cli-validator Public GoFin CLI/broker/engine lifecycle on cloned local programs; requires pinned patched Surfpool' \
	  'make test-fork         Explicit Ethereum fork verification; requires Anvil and archive access' \
	  'make test-cctp-fork    Owned CCTP contract forks; CCTP_FORK_ARGS="--chain 42161" selects one' \
	  'make test-amm-fork     Owned AMM forks; AMM_FORK_ARGS="--dex ramses" tests Robinhood Ramses' \
	  'make test-aave-cli-fork Public CLI named Aave lifecycle; AAVE_FORK_CHAIN=1|8453|42161' \
	  'make test-morpho-rehearsal Public CLI Morpho V2 lifecycle, loss and restart recovery; synthetic evidence' \
	  'make test-morpho-cli-fork Public CLI Morpho V2 lifecycle on an owned fork; MORPHO_FORK_CHAIN=1|8453' \
	  'make test-aave-fork    Owned Aave V3 fork; AAVE_FORK_CHAIN=1|8453|42161, AAVE_FORK_POOL=optional-pool, AAVE_FORK_PIN=saved-pin.json' \
	  'make test-integration  Isolated broker, journal, approval and installer fixtures' \
	  'make test-delivery     Synthetic release and curl-installer tests' \
	  'make test              Positive E2E scenarios plus critical security negatives' \
	  'make check             Rust formatting and strict Clippy' \
	  'make bundle            Build an inspectable debug macOS bundle; do not install' \
	  'make release           Build an optimized macOS release bundle; do not install' \
	  'make setup             HUMAN: build debug bundle and run real protected setup' \
	  'make update            HUMAN: update runtime and existing managed skills; keep connections' \
	  'make uninstall         HUMAN: confirm deletion of installed credentials, config and history'

dev:
	@cargo run --quiet --locked -p gofin-cli -- $(ARGS)

build:
	@cargo build --locked -p gofin-cli -p gofin-broker

dev-env-start:
	@python3 scripts/gofin-dev.py $(DEV_FLAGS) start "$(DEV_ENV)" --mode "$(DEV_MODE)" $(if $(DEV_FIXTURE),--fixture "$(DEV_FIXTURE)")

dev-env-import:
	@test -n "$(DEV_CREDENTIALS_FILE)" || { printf '%s\n' 'Set DEV_CREDENTIALS_FILE to your disposable .env file.' >&2; exit 2; }
	@python3 scripts/gofin-dev.py $(DEV_FLAGS) import "$(DEV_ENV)" --credentials-file "$(DEV_CREDENTIALS_FILE)" $(DEV_IMPORT_FLAGS)

dev-env-cli:
	@python3 scripts/gofin-dev.py cli "$(DEV_ENV)" -- $(ARGS)

dev-env-stop:
	@python3 scripts/gofin-dev.py $(DEV_FLAGS) stop "$(DEV_ENV)"

dev-env-reset:
	@python3 scripts/gofin-dev.py $(DEV_FLAGS) reset "$(DEV_ENV)"

catalog:
	@gofin_catalog_tmp=$$(mktemp); trap 'rm -f "$$gofin_catalog_tmp"' EXIT; \
	  cargo run --quiet --locked -p gofin-broker --example export_catalog > "$$gofin_catalog_tmp" && \
	  mv "$$gofin_catalog_tmp" crates/gofin-cli/data/financial-catalog.json

test-ui:
	@python3 scripts/run-e2e.py --group core
	@python3 scripts/run-security-tests.py --group cli-authority --group misleading-financial-output
	@python3 scripts/test-prepare-amm-touch-id-checklist.py
	@python3 scripts/test-prepare-hyperliquid-human-plan.py
	@$(MAKE) test-native-approval

test-native-approval:
	@if [ "$$(uname -s)" = Darwin ]; then \
	  mkdir -p native/.build && \
	  cargo run --quiet --locked -p gofin-authorization --example export_resolution_presentation > native/.build/operation-resolution-fixture.json && \
	  cargo run --quiet --locked -p gofin-authorization --example export_financial_presentations > native/.build/financial-presentation-fixtures.json && \
	  xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift -o native/.build/approval-presentation-tests && \
	  GOFIN_NATIVE_FINANCIAL_FIXTURES="$(CURDIR)/native/.build/financial-presentation-fixtures.json" GOFIN_NATIVE_RESOLUTION_FIXTURE="$(CURDIR)/native/.build/operation-resolution-fixture.json" native/.build/approval-presentation-tests && \
	  cargo test --locked -p gofin-broker --example gofin-dev-broker actual_jupiter_approval_binding_renders_in_native_review; \
	else \
	  printf '%s\n' 'Native Touch ID presentation tests: not run (requires macOS).'; \
	fi

test-dev-env:
	@python3 scripts/test-gofin-dev.py
	@python3 scripts/test-dev-runtime.py
	@python3 scripts/test-solana-rehearsal.py
	@for scenario in revalidate-once dispatch-once revalidate-every dispatch-every uncertain; do \
	  python3 scripts/test-solana-rehearsal.py --expiry-only "$$scenario" || exit $$?; \
	done
	@python3 scripts/test-solana-rehearsal.py --perps-only
	@python3 scripts/test-solana-rehearsal.py --perps-collateral-only
	@python3 scripts/test-solana-rehearsal.py --perps-limit-only
	@python3 scripts/test-solana-rehearsal.py --perps-limit-plan-only
	@python3 scripts/test-solana-rehearsal.py --perps-tpsl-only
	@python3 scripts/test-solana-rehearsal.py --perps-tpsl-paired-only
	@python3 scripts/test-solana-rehearsal.py --perps-tpsl-plan-only
	@python3 scripts/test-solana-rehearsal.py --jlp-only
	@python3 scripts/test-solana-rehearsal.py --trigger-only
	@python3 scripts/test-solana-rehearsal.py --trigger-fill-only
	@python3 scripts/test-fork-runner.py
	@python3 scripts/test-base-aave-fork-runner.py
	@python3 scripts/test-amm-fork-runner.py
	@python3 scripts/test-cctp-fork-runner.py
	@python3 scripts/test-dev-workflow.py

test-solana-cex-funding:
	@GOFIN_DEV_BUILD_ID=synthetic-solana-funding-build cargo build --offline --locked -p gofin-cli --example gofin-dev
	@GOFIN_SOLANA_FUNDING_CLI="$(CURDIR)/target/debug/examples/gofin-dev" cargo test --offline --locked -p gofin-broker --lib solana_funding

test-coinbase:
	@cargo test --locked -p gofin-coinbase
	@cargo test --locked -p gofin-exchange --features coinbase,test-support coinbase::
	@cargo test --locked -p gofin-broker --lib coinbase
	@GOFIN_DEV_BUILD_ID=synthetic-coinbase-build cargo build --locked -p gofin-cli --example gofin-dev
	@gofin_coinbase_cli_tmp=$$(mktemp); trap 'rm -f "$$gofin_coinbase_cli_tmp"' EXIT; \
	  cp target/debug/examples/gofin-dev "$$gofin_coinbase_cli_tmp" && chmod 700 "$$gofin_coinbase_cli_tmp" && \
	  GOFIN_COINBASE_PLAN_CLI="$$gofin_coinbase_cli_tmp" cargo test --locked -p gofin-broker --test coinbase_plan_workflow --test coinbase_deposit --test coinbase_withdrawal

test-kamino-rehearsal:
	@GOFIN_DEV_BUILD_ID=synthetic-kamino-build cargo build --locked -p gofin-cli --example gofin-dev
	@gofin_kamino_cli_dir=$$(mktemp -d); trap 'rm -rf "$$gofin_kamino_cli_dir"' EXIT; \
	  cp target/debug/examples/gofin-dev "$$gofin_kamino_cli_dir/gofin" && \
	  GOFIN_KAMINO_CLI="$$gofin_kamino_cli_dir/gofin" cargo test --locked -p gofin-broker --test kamino_workflow -- --include-ignored

test-kamino-validator:
	@python3 scripts/verify-kamino-lending-validator.py
	@python3 scripts/verify-kamino-liquidity-validator.py

.PHONY: test-kamino-borrowing-validator
test-kamino-borrowing-validator:
	@GOFIN_DEV_BUILD_ID=synthetic-kamino-build cargo build --locked -p gofin-cli --example gofin-dev
	@cargo test --locked -p gofin-broker --test kamino_borrowing_validator --no-run
	@gofin_klend_cli_dir=$$(mktemp -d); trap 'rm -rf "$$gofin_klend_cli_dir"' EXIT; \
	  cp target/debug/examples/gofin-dev "$$gofin_klend_cli_dir/gofin" && \
	  GOFIN_KAMINO_CLI="$$gofin_klend_cli_dir/gofin" python3 scripts/verify-kamino-borrowing-validator.py

test-meteora-rehearsal:
	@python3 scripts/test-meteora-rehearsal.py

test-aave-mcp-read-cli:
	@python3 scripts/test-aave-mcp-read-cli.py

test-meteora-validator:
	@python3 scripts/verify-meteora-validator.py --family both

test-meteora-cli-validator:
	@python3 scripts/test-meteora-cli-validator.py --family both

.PHONY: test-full-plan-rehearsal
test-full-plan-rehearsal:
	@GOFIN_DEV_BUILD_ID=synthetic-kraken-build cargo build --locked -p gofin-cli --example gofin-dev
	@cp target/debug/examples/gofin-dev target/debug/examples/gofin-full-plan-rehearsal
	@GOFIN_KRAKEN_PLAN_CLI="$(CURDIR)/target/debug/examples/gofin-full-plan-rehearsal" cargo test --locked -p gofin-broker --test full_plan_workflow

test-papertrade-rehearsal:
	@cargo test --locked -p gofin-exchange --features hyperliquid ccxt::hyperliquid_tests::papertrade_ -- --nocapture
	@GOFIN_DEV_BUILD_ID=synthetic-papertrade-workflow-build cargo build --locked -p gofin-cli --example gofin-dev
	@GOFIN_PAPERTRADE_CLI="$(CURDIR)/target/debug/examples/gofin-dev" cargo test --locked -p gofin-broker --test papertrade_workflow -- --include-ignored
	@cargo test --locked -p gofin-broker --lib broker_runtime_signs_submits_and_canonically_verifies_session_registration -- --nocapture
	@GOFIN_PAPERTRADE_CLI="$(CURDIR)/target/debug/examples/gofin-dev" cargo test --locked -p gofin-broker --lib papertrade_funding_host::tests::core_prepare_dispatch_and_sweep_settle_through_the_real_funding_host -- --exact --nocapture
	@GOFIN_PAPERTRADE_CLI="$(CURDIR)/target/debug/examples/gofin-dev" cargo test --locked -p gofin-broker --lib papertrade_funding_host::tests::core_withdrawal_named_plan_uses_real_runtime_and_canonical_credit_proof -- --exact --ignored --nocapture
	@GOFIN_PAPERTRADE_CLI="$(CURDIR)/target/debug/examples/gofin-dev" cargo test --locked -p gofin-broker --lib papertrade_funding_host::tests::hyperevm_withdrawal_named_plan_settles_core_stage_without_dispatching_onward_leg -- --exact --ignored --nocapture
	@GOFIN_PAPERTRADE_CLI="$(CURDIR)/target/debug/examples/gofin-dev" GOFIN_PAPERTRADE_RECOVERY_REHEARSAL=success cargo test --locked -p gofin-broker --lib papertrade_funding_host::tests::core_prepare_dispatch_and_sweep_settle_through_the_real_funding_host -- --exact --nocapture
	@GOFIN_PAPERTRADE_CLI="$(CURDIR)/target/debug/examples/gofin-dev" GOFIN_PAPERTRADE_RECOVERY_REHEARSAL=lost_noop cargo test --locked -p gofin-broker --lib papertrade_funding_host::tests::core_prepare_dispatch_and_sweep_settle_through_the_real_funding_host -- --exact --nocapture
	@cargo test --locked -p gofin-broker --lib papertrade_funding_host::tests::core_lost_response_reconciles_original_nonce_and_persists_hash_without_resend -- --exact

.PHONY: test-morpho-rehearsal test-morpho-cli-fork
test-morpho-rehearsal:
	@GOFIN_DEV_BUILD_ID=synthetic-morpho-workflow-build cargo build --locked -p gofin-cli --example gofin-dev
	@cp target/debug/examples/gofin-dev target/debug/examples/gofin-morpho-rehearsal
	@GOFIN_MORPHO_CLI="$(CURDIR)/target/debug/examples/gofin-morpho-rehearsal" cargo test --locked -p gofin-broker --test morpho_workflow -- --include-ignored

MORPHO_FORK_CHAIN ?= 1
MORPHO_FORK_PIN ?=
MORPHO_FORK_RPC_URL ?=
test-morpho-cli-fork:
	@python3 scripts/verify-morpho-cli-fork.py --chain "$(MORPHO_FORK_CHAIN)" $(if $(MORPHO_FORK_PIN),--pin "$(MORPHO_FORK_PIN)",) $(if $(MORPHO_FORK_RPC_URL),--rpc-url "$(MORPHO_FORK_RPC_URL)",)

test-fork:
	@/bin/sh scripts/verify-rust-fork.sh

test-amm-fork:
	@python3 scripts/verify-amm-fork.py $(AMM_FORK_ARGS)

test-integration: test-dev-env
	@cargo test --locked -p gofin-broker -p gofin-engine -p gofin-store -p gofin-authorization -p gofin-human-terminal
	@python3 scripts/test-rust-macos-installer.py
	@python3 scripts/test-runtime-store-compatibility.py
	@python3 scripts/test-rust-macos-runtime-update.py
	@python3 scripts/test-rust-macos-uninstall.py

test-delivery:
	@python3 scripts/test-installer-site.py
	@python3 scripts/test-curl-installer.py
	@python3 scripts/test-release-build.py
	@python3 scripts/test-release-upload.py

test: test-security test-e2e

# Explicit positive public-CLI acceptance. Missing scenarios/prerequisites fail.
.PHONY: test-e2e test-e2e-list test-cli-regressions test-security
test-security:
	@python3 scripts/run-security-tests.py $(SECURITY_ARGS)

test-e2e:
	@python3 scripts/run-e2e.py $(E2E_ARGS)

test-e2e-list:
	@python3 scripts/run-e2e.py --list $(E2E_ARGS)

test-cli-regressions:
	@python3 scripts/e2e/verify-cli-regressions.py

check:
	@cargo fmt --all -- --check
	@cargo clippy --locked --workspace --all-targets -- -D warnings

bundle:
	@/bin/sh scripts/dev-macos.sh bundle

release:
	@/bin/sh scripts/dev-macos.sh release

setup:
	@/bin/sh scripts/dev-macos.sh setup

update:
	@/bin/sh scripts/dev-macos.sh update

uninstall:
	@/bin/sh scripts/dev-macos.sh uninstall

# Explicit owned Aave fork; no secret import or remote signing.
test-aave-fork:
	@python3 scripts/verify-aave-v3-fork.py --chain "$(AAVE_FORK_CHAIN)" $(if $(AAVE_FORK_POOL),--pool "$(AAVE_FORK_POOL)",) $(if $(AAVE_FORK_PIN),--pin "$(AAVE_FORK_PIN)",)

# Public CLI/engine named lifecycle on one owned deployed-state fork.
.PHONY: test-aave-cli-fork
test-aave-cli-fork:
	@python3 scripts/test-aave-fork-receipt-proxy.py
	@python3 scripts/verify-aave-cli-plan-fork.py --chain "$(AAVE_FORK_CHAIN)" $(if $(AAVE_FORK_PIN),--pin "$(AAVE_FORK_PIN)",)

.PHONY: test-cctp-fork
test-cctp-fork:
	@python3 scripts/verify-cctp-fork.py $(CCTP_FORK_ARGS)

# Real Binance adapter behind an isolated persistent exchange transport.
test-binance-futures-rehearsal:
	@python3 scripts/test-binance-futures-rehearsal.py
