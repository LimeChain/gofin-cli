#!/usr/bin/env python3
"""Run Kamino CLI scenarios with fresh, natively loaded Surfpool fixtures."""
from __future__ import annotations

import argparse
import importlib.util
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
TOKEN = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
ATA = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
PROGRAMS = {
    "borrowing": ["KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD", "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"],
    "lending": ["KvauGMspG5k6rtzrqqn7WNn3oZdyKqLKwK2XWQ8FLjd", "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD", "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"],
    "liquidity": ["6LtLpnUFNByNXLyCoK9wA2MykKAmQNZKBdY8s47dehDc", "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc", "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"],
}
COMMON = [TOKEN, "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", ATA,
          "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


native = module("surfpool_native", "scripts/e2e/surfpool-native.py")

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("family", choices=PROGRAMS)
    parser.add_argument("--cli", required=True)
    parser.add_argument("--summary", required=True, type=Path)
    args = parser.parse_args()
    summary = args.summary.resolve()
    summary.parent.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=f"kamino-{args.family}-", dir=summary.parent))
    evidence = {"scenario": f"kamino-{args.family}", "passed": False,
        "selected_tests": 0, "environment": "synthetic_surfpool",
        "public_rpc": "read_only_by_surfpool", "submission_rpc": "loopback",
        "fresh_environment": str(directory), "fixture_loading": "surfpool_native"}
    try:
        module("surfpool_validation", "scripts/test-meteora-cli-validator.py").validate_surfpool()
        discovery = module("kamino_discovery", "scripts/e2e/kamino-prepare.py")
        with native.Surfpool(directory / "discovery") as node:
            discovered = discovery.discover(args.family, node.rpc, directory / "decoder")
        addresses = sorted(set(discovered["accounts"] + PROGRAMS[args.family] + COMMON))
        # Surfpool sends this list as one native getMultipleAccounts request.
        # Never split consistency-sensitive reserve/vault state across batches.
        if len(addresses) > 100:
            raise RuntimeError("Kamino dependency graph exceeds one native account batch")
        snapshot = directory / "dependencies.json"
        native.save(snapshot, dict.fromkeys(addresses))
        with native.Surfpool(directory / "execution", snapshot) as node:
            loaded = native.validate_preload(node.rpc, addresses,
                PROGRAMS[args.family] + COMMON, directory / "preload-evidence.json")
            manifest = discovery.discover(args.family, native.cached_rpc(loaded), directory / "decoder")
            if manifest != discovered:
                raise RuntimeError("Kamino identities or dependency graph changed during native loading")
            discovery.apply_policies(args.family, node.rpc, manifest)
            owner, key = native.wallet(node.rpc, directory, "wallet",
                                100_000_000_000 if args.family == "liquidity" else 1_000_000_000_000)
            manifest.update(wallet=owner, wallet_key=str(key),
                            fixture_loading="surfpool_native", upstream_context_slot=None)
            balances = [native.fund_token(node.rpc, owner, asset, 1000 * 10 ** asset["decimals"])
                        for asset in manifest["assets"]]
            if args.family == "liquidity":
                balances.append(native.fund_token(node.rpc, owner, manifest["share_asset"], 0))
            env = {**os.environ, "GOFIN_KAMINO_CLI": str(Path(args.cli).resolve()),
                "GOFIN_KAMINO_POSITIVE_RPC": node.url, "GOFIN_SURFPOOL_ADVANCE": "1"}
            if args.family == "borrowing":
                second, second_key = native.wallet(node.rpc, directory, "second-wallet", 1_000_000_000)
                balances.append(native.fund_token(node.rpc, second, manifest["assets"][0], 0))
                env["GOFIN_KAMINO_SECOND_KEY"] = str(second_key)
                manifest["second_wallet"] = second
            native.save(directory / "initial-balances.json", balances)
            manifest_path = directory / "manifest.json"
            native.save(manifest_path, manifest)
            env["GOFIN_KAMINO_POSITIVE_MANIFEST"] = str(manifest_path)
            result = subprocess.run(["cargo", "test", "--locked", "-p", "gofin-broker",
                "--test", "kamino_positive_validator", f"positive_kamino_{args.family}_via_public_cli",
                "--", "--ignored", "--exact", "--nocapture"], cwd=ROOT, env=env,
                text=True, capture_output=True)
            print(result.stdout, end="")
            print(result.stderr, end="", file=sys.stderr)
            matches = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", result.stdout)
            passed = result.returncode == 0 and matches == [("1", "0", "0")]
            evidence.update(passed=passed, selected_tests=1 if passed else 0,
                            dependency_count=len(addresses), wallet=owner)
            return 0 if passed else 1
    finally:
        native.save(summary, evidence)


if __name__ == "__main__":
    sys.exit(main())
