.DEFAULT_GOAL := help
SHELL := /bin/sh
ARGS ?= --help

.PHONY: help dev build test-ui test-integration test-delivery test check bundle release setup update uninstall

help:
	@printf '%s\n' \
	  'make dev [ARGS="..."]  Run the debug CLI (default: help; no installation)' \
	  'make build             Incrementally build the debug CLI and broker' \
	  'make test-ui           Offline CLI, presentation and discovery tests' \
	  'make test-integration  Isolated broker, journal, approval and installer fixtures' \
	  'make test-delivery     Synthetic release and curl-installer tests' \
	  'make test              Full offline Rust test suite' \
	  'make check             Rust formatting and strict Clippy' \
	  'make bundle            Build an inspectable debug macOS bundle; do not install' \
	  'make release           Build an optimized macOS release bundle; do not install' \
	  'make setup             HUMAN: build debug bundle and run real protected setup' \
	  'make update            HUMAN: build and update runtime; refresh existing managed skills' \
	  'make uninstall         HUMAN: delete installed credentials/config; refuse financial history'

dev:
	@cargo run --quiet --locked -p fin-cli -- $(ARGS)

build:
	@cargo build --locked -p fin-cli -p fin-broker

test-ui:
	@cargo test --locked -p fin-cli -p fin-discovery -p fin-presentation --lib --bins --test offline_cli

test-integration:
	@cargo test --locked -p fin-broker -p fin-engine -p fin-store -p fin-authorization -p fin-human-terminal
	@python3 scripts/test-rust-macos-installer.py
	@python3 scripts/test-rust-macos-runtime-update.py
	@python3 scripts/test-rust-macos-uninstall.py
	@python3 scripts/test-dev-workflow.py

test-delivery:
	@python3 scripts/test-curl-installer.py
	@python3 scripts/test-release-build.py
	@python3 scripts/test-release-upload.py

test:
	@cargo test --locked --workspace

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
