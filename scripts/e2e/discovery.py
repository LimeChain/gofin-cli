#!/usr/bin/env python3
"""Positive public-CLI discovery, isolated from installed broker and user state."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
OPERATIONS = [
    "wallet.transfer", "wallet.approve", "wallet.wrap", "wallet.unwrap",
    "wallet.permit2.allowance", "wallet.permit2.approve",
    "binance.deposit", "binance.spot.order-place", "binance.futures.order-place",
    "binance.withdraw", "kraken.add-order", "kraken.cancel-order",
    "krakenfutures.send-order", "coinbase.create-order", "coinbase.deposit",
    "coinbase.withdraw", "coinbaseexchange.create-order", "coinbaseinternational.create-order",
    "hyperliquid.orders.place", "hyperliquid.account.deposit", "hyperliquid.account.withdraw",
    "uniswap.swap", "sushi.quote", "sushi.swap", "pancakeswap.quote", "pancakeswap.swap", "ramses.quote", "ramses.swap",
    "aave.markets", "aave.supply", "aave.borrow", "aave.repay", "aave.withdraw",
    "morpho.deposit", "morpho.withdraw", "morpho.redeem",
    "jupiter.swap", "jupiter.jlp.mint", "jupiter.jlp.burn",
    "jupiter.perps.increase_market", "jupiter.perps.decrease_market",
    "meteora.position.open", "meteora.swap", "meteora.position.close",
    "kamino.deposit", "kamino.withdraw", "kamino.lending.borrow", "kamino.lending.repay",
    "bridge.routes", "bridge.quotes", "bridge.execute", "bridge.status",
    "papertrade.open", "papertrade.close", "papertrade.stake", "papertrade.claim",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    args = parser.parse_args()
    binary = args.cli.resolve(strict=True)
    exported = subprocess.run(
        ["cargo", "run", "--quiet", "--locked", "-p", "gofin-broker", "--example", "export_catalog"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    compiled = json.loads(exported.stdout)
    command = [str(binary)]
    if sys.platform == "darwin":
        sandbox = shutil.which("sandbox-exec")
        assert sandbox, "macOS discovery isolation requires sandbox-exec"
        # Do not inspect/contact the installed service while exercising the real
        # production parser. The CLI treats failed nonactivating observation as offline.
        command = [sandbox, "-p", '(version 1)(allow default)'
                   '(deny file-read* file-write* (subpath "/Library/GoFin"))'
                   '(deny process-exec (literal "/bin/launchctl"))'
                   '(deny network-outbound)', str(binary)]
    with tempfile.TemporaryDirectory(prefix="gofin-discovery-e2e-") as home:
        env = dict(os.environ, HOME=home, NO_COLOR="1", TERM="dumb", XDG_CONFIG_HOME=home)

        def call(*argv, machine=True):
            output = subprocess.run(command + list(argv), env=env, stdin=subprocess.DEVNULL,
                                    capture_output=True, text=True, timeout=30)
            assert output.returncode == 0, (argv, output.returncode, output.stdout, output.stderr)
            assert not output.stderr, (argv, output.stderr)
            assert "\x1b" not in output.stdout, (argv, "ANSI decoration")
            return json.loads(output.stdout) if machine else output.stdout

        help_text = call("--help", machine=False)
        for name in ["capabilities", "explain", "schema", "preview", "execute", "connections", "authorizations"]:
            assert name in help_text, f"Missing public command {name}"
        caps = call("capabilities", "--json")
        assert caps["service_required"] is False
        assert caps["aave_mcp"]["status"] == "broker_required", "Discovery unexpectedly contacted a service"
        assert caps["commands"], "Empty command catalog"
        actual = {row["name"]: row for row in caps["operations"]}
        assert len(actual) == len(caps["operations"]), "Duplicate operation identity"
        expected = json.loads((ROOT / "crates/gofin-cli/data/financial-catalog.json").read_text())
        assert actual == {row["name"]: row for row in expected}, "Public financial catalog changed or is incomplete"
        assert actual == {row["name"]: row for row in compiled}, "Bundled catalog differs from compiled broker registry"
        exchanges = caps["exchanges"]
        for venue in ["kraken", "krakenfutures", "coinbase", "coinbaseexchange", "coinbaseinternational"]:
            assert exchanges["gofin"][venue]["writes"] is True
            assert exchanges["gofin"][venue]["account_enrollment"] is True
            assert exchanges["gofin"][venue]["public_only"] is False
        for venue in ["okx", "bybit"]:
            assert exchanges["gofin"][venue]["writes"] is False
            assert exchanges["gofin"][venue]["account_enrollment"] is False
        assert exchanges["library"]["all_library_methods_exposed"] is False
        assert exchanges["verification"]["touch_id"] == "pending"
        networks = {row["chain_id"]: row for row in caps["evm_networks"]}
        assert networks[8453]["name"] == "Base" and 42161 in networks
        for name in ["exchange.markets", "exchange.ticker"]:
            schema = call("schema", name, "--json")
            assert schema["input"]["additionalProperties"] is False
            assert schema["execution_actor"] == "agent_readable"
            assert {"kraken", "hyperliquid", "okx", "coinbase", "bybit"} <= set(schema["input"]["properties"]["exchange"]["enum"])
        human_capabilities = call("capabilities", machine=False)
        for name in ["connections.add", "wallet.transfer", "jupiter.swap"]:
            assert name in human_capabilities
        assert "input_schema:" not in human_capabilities
        for name in ["setup", "connections.add", "plans.preview", *OPERATIONS]:
            definition = call("explain", name, "--json")
            schema = call("schema", name, "--json")
            assert definition["name"] == schema["name"] == name
            for key in ["input", "output", "error"]:
                assert schema[key] == definition[f"{key}_schema"], (name, key)
            assert definition["description"], f"Missing explanation for {name}"
            assert isinstance(schema["input"], dict), f"No input schema for {name}"
        human = call("explain", "jupiter.swap", machine=False)
        assert human.strip() and "jupiter.swap" in human
        assert "Use --verbose" in human and "input_schema:" not in human
        verbose = call("explain", "jupiter.swap", "--verbose", machine=False)
        assert "input_schema:" in verbose and "minimum_output_atoms" in verbose
        enrollment = call("explain", "connections.add", machine=False)
        assert "sudo" in enrollment and "gofin connections add" in enrollment
        for name, actor in [("authorizations.authorize", "agent_invocable"),
                            ("execute.boundary", "agent_invocable"),
                            ("authorizations.status", "agent_readable"),
                            ("authorizations.cancel", "agent_invocable")]:
            definition = call("explain", name, "--json")
            assert definition["execution_actor"] == actor
            assert isinstance(definition["semantics"], dict)
        authority = call("schema", "authorizations.authorize", "--json")
        assert authority["semantics"]["requires_fresh_human_approval"] is True
        assert authority["semantics"]["human_terminal_review"] is True
        assert {"operation", "id", "input", "bounds", "valid_for", "replaces"} <= authority["input"]["properties"].keys()
        boundary = call("schema", "execute.boundary", "--json")
        assert {"operation", "input", "authorization", "id", "replaces_attempt", "dry_run"} <= boundary["input"]["properties"].keys()
        assert "unsubmitted" in boundary["semantics"]["candidate_replacement"]
        assert not any(path.is_file() for path in Path(home).rglob("*.sqlite")), "Discovery initialized financial state"
    print(f"PASS cli.discovery: {len(actual)} operations; {len(OPERATIONS) + 3} explain/schema roundtrips; offline human and JSON output")


if __name__ == "__main__":
    main()
