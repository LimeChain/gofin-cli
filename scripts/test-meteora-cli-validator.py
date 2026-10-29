#!/usr/bin/env python3
"""Exercise the public GoFin CLI and engine against cloned Meteora programs.

The validator is local, the enrolled wallet and auxiliary signers are synthetic,
and Surfpool owns read-only dependency loading through its native APIs.
The script never reads .env or production credentials. Private execution evidence
is written under ignored .context/meteora-cli-validator/.
"""

from __future__ import annotations

import importlib.util
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import stat
import socket
import subprocess
import sys
import time
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = ROOT / ".context" / "meteora-validator"
CLI_EVIDENCE = ROOT / ".context" / "meteora-cli-validator"
DEV = ROOT / "scripts" / "gofin-dev.py"
FAMILY_POOLS = {
    "dlmm": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
    "damm_v2": "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie",
}
WSOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
WALLET_ALIAS = "meteora-validator"
ACTIVE_EVIDENCE: dict | None = None


def load_validator_helpers():
    path = ROOT / "scripts" / "verify-meteora-validator.py"
    spec = importlib.util.spec_from_file_location("meteora_validator_setup", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("Could not load the local-validator setup helper")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(args: list[str], env: dict[str, str], *, timeout: int = 180) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=ROOT, env=env, capture_output=True, text=True, timeout=timeout)


def json_command(args: list[str], env: dict[str, str], *, timeout: int = 180) -> dict:
    result = run(args, env, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{' '.join(args)} failed: {result.stdout}\n{result.stderr}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"Command did not return JSON: {result.stdout!r} {result.stderr!r}") from error


def cli(env: dict[str, str], name: str, *args: str, timeout: int = 180) -> dict:
    return json_command(
        [sys.executable, str(DEV), "cli", name, "--", *args], env, timeout=timeout
    )


def command_count(rpc, url: str, wallet: str) -> int:
    rows = rpc(url, "getSignaturesForAddress", [wallet, {"limit": 1000}])
    return len(rows)


def private_directory(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise RuntimeError(f"Private evidence directory is not an owned plain directory: {path}")
    os.chmod(path, 0o700)


def load_native_surfpool():
    spec = importlib.util.spec_from_file_location("surfpool_native", ROOT / "scripts/e2e/surfpool-native.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def validate_discovered_manifest(discovered: dict, execution: dict) -> None:
    """Bind every dependency identity while allowing price movement in one array."""
    changed = {key for key in discovered.keys() | execution.keys()
               if key not in discovered or key not in execution
               or discovered[key] != execution[key]}
    if discovered.get("family") == execution.get("family") == "dlmm":
        # These are two independent native upstream fetches. The active bin is
        # market state, not an account identity; it may move before preloading.
        # Crossing an array boundary or changing any graph/asset field still
        # fails, and both decoded bin IDs must agree with their array identity.
        for manifest in (discovered, execution):
            active = manifest.get("active_bin")
            array = manifest.get("active_bin_array")
            if type(active) is not int or type(array) is not int or active // 70 != array:
                raise RuntimeError("Meteora active bin does not match its dependency array")
        changed.discard("active_bin")
    if changed:
        raise RuntimeError("Meteora identities or dependency graph changed during native loading: "
                           + ", ".join(sorted(changed)))


def start_validator(helper, port: int, family: str):
    """Discover locally, then let a fresh Surfpool natively preload the graph."""
    native = load_native_surfpool()
    private_directory(EVIDENCE)
    directory = Path(tempfile.mkdtemp(prefix=f"native-{family}-", dir=EVIDENCE))
    spec = importlib.util.spec_from_file_location("meteora_discovery", ROOT / "scripts/e2e/prepare-meteora.py")
    discovery = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(discovery)
    with native.Surfpool(directory / "discovery") as node:
        manifest = discovery.discover(family, node.rpc)
    native.save(directory / "discovery-manifest.json", manifest)
    programs = [manifest["program"], *helper.PROGRAMS[2:], helper.MEMO, helper.ATA]
    addresses = sorted(set(manifest["accounts"] + programs))
    if len(addresses) > 100:
        raise RuntimeError("Meteora dependencies exceed one native account batch")
    snapshot = directory / "dependencies.json"
    native.save(snapshot, dict.fromkeys(addresses))
    node = native.Surfpool(directory / "execution", snapshot, rpc_port=port)
    node.__enter__()
    try:
        loaded = native.validate_preload(node.rpc, addresses, programs, directory / "preload-evidence.json")
        execution_manifest = discovery.discover(family, native.cached_rpc(loaded))
        native.save(directory / "execution-manifest.json", execution_manifest)
        validate_discovered_manifest(manifest, execution_manifest)
        manifest = execution_manifest
        owner, key = native.wallet(node.rpc, directory, "wallet", 1_000_000_000_000)
        identities = {"wallet": {"pubkey": owner, "path": str(key)}}
        balances = []
        for asset, label, amount in zip(manifest["assets"], ("wsol_ata", "usdc_ata"), (10_000_000_000, 1_000_000_000_000_000)):
            balance = native.fund_token(node.rpc, owner, asset, amount)
            balances.append(balance)
            identities[label] = {"pubkey": balance["address"], **asset}
        native.save(directory / "initial-balances.json", balances)
        native.save(directory / "manifest.json", manifest)
        native.save(directory / "synthetic-identities.json", identities)
        return node, directory, identities
    except BaseException:
        node.__exit__(None, None, None)
        raise


def start_environment(env: dict[str, str], name: str) -> dict:
    print(f"building isolated broker snapshot for {name}", flush=True)
    # A cold build of both examples can exceed the transaction-command budget.
    # Keep that budget unchanged; only environment construction gets more time.
    return json_command([
        "make", "dev-env-start", f"DEV_ENV={name}", "DEV_MODE=synthetic",
        "DEV_FIXTURE=solana-validator", "DEV_FLAGS=--json",
    ], env, timeout=600)


def stop_environment(env: dict[str, str], name: str) -> None:
    json_command(["make", "dev-env-stop", f"DEV_ENV={name}", "DEV_FLAGS=--json"], env)


def quote(env: dict[str, str], name: str, operation: str, request: dict) -> dict:
    return cli(env, name, "read", "--input", json.dumps({"action": operation, **request}), "meteora.quote", "--json")


def execute(env: dict[str, str], name: str, operation: str, request: dict, label: str) -> dict:
    run_id = (ACTIVE_EVIDENCE or {}).get("run_id")
    if not isinstance(run_id, str) or not run_id:
        raise RuntimeError("Public lifecycle operation has no durable test run identity")
    result = cli(
        env, name, "execute", operation, "--input", json.dumps(request),
        "--id", f"meteora-validator-{run_id}-{label}", "--json",
    )
    operation_id = result.get("operation_id") or result.get("id")
    if operation_id and result.get("status") in {"submitted", "pending", "uncertain", "reconciling"}:
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            result = cli(env, name, "status", str(operation_id), "--json", timeout=180)
            if result.get("status") not in {"submitted", "pending", "uncertain", "reconciling"}:
                break
            time.sleep(1)
    if result.get("status") != "completed":
        raise AssertionError(f"{label} did not settle through the public engine: {json.dumps(result, sort_keys=True)}")
    preview = result.get("preview") or {}
    if not preview.get("summary") or not (preview.get("intent") or {}).get("maximum_costs"):
        raise AssertionError(f"{label} omitted the human review summary or maximum costs")
    if (result.get("approval") or {}).get("source") != "development":
        raise AssertionError(f"{label} omitted its isolated development approval")
    slot = (result.get("last_reconciliation_evidence") or {}).get("slot")
    if not isinstance(slot, int):
        raise AssertionError(f"{label} omitted its finalized transaction slot")
    # Start the next baseline strictly after this write. A baseline in the same
    # slot cannot prove that its position delta is isolated from this write.
    wait = load_native_surfpool().advance_finalized_slot(
        env["GOFIN_METEORA_VALIDATOR_RPC"], slot)
    print("SURFPOOL_FINALIZATION_WAIT " + json.dumps({"label": label, **wait}), flush=True)
    return result


def effect_rows(operation_result: dict) -> list[dict]:
    verified = operation_result.get("effects") or {}
    return verified.get("effects", []) if isinstance(verified, dict) else []


def rent_change(operation_result: dict) -> int:
    total = 0
    for effect in effect_rows(operation_result):
        if effect.get("kind") != "position_change" or not str(effect.get("position", "")).startswith("account-funding:"):
            continue
        try:
            total += int(effect.get("delta", "0"))
        except (TypeError, ValueError):
            raise AssertionError("Verified rent position change did not contain exact signed lamports")
    return total


def positive_usable_credits(operation_result: dict) -> list[dict]:
    credits = []
    for effect in effect_rows(operation_result):
        if effect.get("kind") != "usable_credit":
            continue
        atoms = (effect.get("amount") or {}).get("atoms", "0")
        try:
            if int(atoms) > 0:
                credits.append(effect)
        except (TypeError, ValueError):
            raise AssertionError("Verified Collect credit did not contain an exact token amount")
    return credits


def positive_collected_claims(operation_result: dict) -> list[dict]:
    claims = []
    for effect in effect_rows(operation_result):
        if effect.get("kind") != "position_change":
            continue
        evidence = effect.get("anchor", {}).get("evidence", {}).get("liquidity_position", {})
        for event in evidence.get("history", []):
            if event.get("action") != "collect":
                continue
            for field in ("fees", "rewards"):
                for claim in event.get(field, []):
                    try:
                        if int(claim.get("atoms", "0")) > 0:
                            claims.append({"kind": field[:-1], **claim})
                    except (TypeError, ValueError):
                        raise AssertionError("Finalized Collect history did not contain an exact claim amount")
    return claims


def assert_zero_collect_quote(quote_result: dict) -> None:
    quote = quote_result.get("quote") or {}
    for field in (
        "fees_a_atoms", "fees_b_atoms",
        "token_a_credit_atoms", "token_b_credit_atoms",
    ):
        value = quote.get(field)
        try:
            if value is None or int(value) != 0:
                raise AssertionError(f"Post-Collect read-only quote expected exact zero {field}: {value!r}")
        except (TypeError, ValueError) as error:
            raise AssertionError(f"Post-Collect read-only quote omitted exact {field}") from error
    if quote.get("rewards") != []:
        raise AssertionError("Post-Collect read-only quote reported unclaimed rewards")


def token_caps(pool: dict, *, wsol_atoms: str, usdc_atoms: str) -> tuple[str, str, str, str]:
    a, b = pool["token_a"], pool["token_b"]
    amounts = {WSOL: wsol_atoms, USDC: usdc_atoms}
    if a["mint"] not in amounts or b["mint"] not in amounts:
        raise AssertionError("Cloned pool is not the expected WSOL/USDC pool")
    return a["mint"], b["mint"], amounts[a["mint"]], amounts[b["mint"]]


def assert_liquidity_snapshot(
    snapshot: dict,
    *,
    action: str,
    position_address: str,
    minimum_events: int,
    require_two_sided_backing: bool = False,
) -> dict:
    # Other application observers may time out; preserve that incomplete
    # coverage in the captured snapshot while checking this exact LP anchor.
    state = snapshot.get("state") or {}
    row = next((
        candidate for candidate in state.get("positions", [])
        if candidate.get("kind") == "liquidity"
        and candidate.get("anchor", {}).get("evidence", {}).get("liquidity_position", {}).get("position") == position_address
    ), None)
    if row is None:
        raise AssertionError(f"{action} snapshot omitted the exact Meteora LP anchor")
    evidence = row["anchor"]["evidence"]["liquidity_position"]
    if not evidence.get("liquidity_units_known") or not evidence.get("liquidity_units_atoms"):
        raise AssertionError(f"{action} snapshot omitted exact stable liquidity units")
    if evidence.get("token_a", {}).get("atoms") is None or evidence.get("token_b", {}).get("atoms") is None:
        raise AssertionError(f"{action} snapshot treated current A/B backing as unknown")
    if require_two_sided_backing and (
        int(evidence["token_a"]["atoms"]) <= 0 or int(evidence["token_b"]["atoms"]) <= 0
    ):
        raise AssertionError(f"{action} did not produce nonzero backing in both pool tokens")
    if not evidence.get("backing_valuation"):
        raise AssertionError(f"{action} snapshot omitted its current A/B backing valuation")
    if len(evidence.get("history", [])) < minimum_events:
        raise AssertionError(f"{action} snapshot lost durable liquidity history")
    receipt_mint = row["anchor"]["evidence"].get("receipt_mint")
    if receipt_mint and any(
        holding.get("amount", {}).get("asset", {}).get("reference") == receipt_mint
        for holding in state.get("holdings", [])
    ):
        raise AssertionError(f"{action} snapshot counted the position NFT as fungible holdings")
    return evidence


def run_family(helper, family: str, port: int) -> dict:
    global ACTIVE_EVIDENCE
    node, directory, identities = start_validator(helper, port, family)
    run_id = uuid.uuid4().hex[:12]
    name = f"meteora-val-{family}-{port}-{run_id}"
    state = CLI_EVIDENCE / f"state-{family}-{port}-{run_id}"
    env = dict(os.environ)
    env.update({
        "GOFIN_DEV_STATE_HOME": str(state),
        "GOFIN_SURFPOOL_ADVANCE": "1",
        "GOFIN_METEORA_VALIDATOR_RPC": f"http://127.0.0.1:{port}",
        "GOFIN_METEORA_VALIDATOR_WALLET_KEY": str((ROOT / identities["wallet"]["path"]).resolve()),
    })
    rpc_url = node.url
    log_path = node.directory / "surfpool.log"
    started = False
    rows: dict[str, object] = {
        "family": family,
        "run_id": run_id,
        "state_root": str(state.relative_to(ROOT)),
        "evidence_class": "public_cli_broker_engine_local_validator",
        "live_write": False,
        "rpc": rpc_url,
        "wallet": identities["wallet"]["pubkey"],
        "validator_log": str(log_path.relative_to(ROOT)),
        "fresh_environment": str(directory.relative_to(ROOT)),
        "fixture_loading": "surfpool_native",
        "operations": {},
    }
    ACTIVE_EVIDENCE = rows
    try:
        rows["start"] = start_environment(env, name)
        print(f"isolated broker snapshot ready for {name}; source freeze released", flush=True)
        started = True
        balance = cli(env, name, "read", "--input", json.dumps({
            "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
        }), "wallet.balances", "--json")
        if not balance.get("wallet") or balance.get("observed_commitment") != "finalized":
            raise AssertionError("Public wallet read omitted finalized wallet identity")
        pool_address = FAMILY_POOLS[family]
        pool_response = cli(env, name, "read", "--input", json.dumps({"family": family, "pool": pool_address}), "meteora.pool", "--json")
        pool = pool_response["pool"]
        a_mint, b_mint, open_a, open_b = token_caps(pool, wsol_atoms="3000000000", usdc_atoms="300000000")
        rows["operations"]["pool"] = pool_response
        rows["operations"]["positions_before"] = cli(env, name, "read", "--input", json.dumps({"wallet": WALLET_ALIAS, "family": family}), "meteora.positions", "--json")
        if not rows["operations"]["positions_before"].get("complete") or rows["operations"]["positions_before"].get("positions") != []:
            raise AssertionError("Fresh validator wallet did not start with a complete empty Meteora position set")
        snapshot_before = cli(env, name, "read", "--input", "{}", "gofin.snapshot", "--json")
        rows["operations"]["snapshot_before"] = snapshot_before

        # Refresh the active bin immediately before Open. The fee-generating
        # swap runs only after the new position has liquidity in this range.
        pool_response = cli(env, name, "read", "--input", json.dumps({"family": family, "pool": pool_address}), "meteora.pool", "--json")
        pool = pool_response["pool"]
        a_mint, b_mint, open_a, open_b = token_caps(pool, wsol_atoms="3000000000", usdc_atoms="300000000")
        rows["operations"]["pool_before_open"] = pool_response

        # A bounded, quote-derived swap executes against the cloned deployed
        # program after the position exists, so it accrues fees to that LP.
        swap = {
            "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
            "family": family, "pool": pool_address, "input_mint": USDC,
            "output_mint": "native", "amount_atoms": "100000000",
            "maximum_slippage_bps": 100, "maximum_network_fee_lamports": 20000,
            "maximum_account_funding_lamports": 2500000,
        }
        open_request = {
            "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
            "family": family, "pool": pool_address,
            "token_a_mint": a_mint, "token_b_mint": b_mint,
            "token_a_atoms": open_a, "token_b_atoms": open_b,
            "maximum_slippage_bps": 100, "maximum_network_fee_lamports": 20000,
            "maximum_account_funding_lamports": 500000000,
        }
        if family == "dlmm":
            active = int(pool["current_bin"])
            lower, upper = active - 1, active + 1
            open_request.update({
                "bin_lower": lower,
                "bin_upper": upper,
                "liquidity_distribution": [
                    {"bin_id": lower, "distribution_x": 0, "distribution_y": 5000},
                    {"bin_id": active, "distribution_x": 5000, "distribution_y": 5000},
                    {"bin_id": upper, "distribution_x": 5000, "distribution_y": 0},
                ],
            })
        rows["operations"]["open_quote"] = quote(env, name, "open", open_request)
        open_result = execute(env, name, "meteora.position.open", open_request, f"{family}-open")
        candidate = (open_result.get("preview") or {}).get("binding", {})
        protocol_candidate = candidate.get("protocol_candidate", {})
        position = protocol_candidate.get("stable_position_address")
        if not position:
            raise AssertionError(f"Open preview omitted stable position identity: {json.dumps(open_result, sort_keys=True)}")
        rows["operations"]["open"] = open_result
        if rent_change(open_result) <= 0:
            raise AssertionError("Open did not settle its exact refundable account-funding anchors")
        rows["operations"]["position_identity"] = position
        rows["operations"]["position_after_open"] = cli(env, name, "read", "--input", json.dumps({"family": family, "position": position}), "meteora.position", "--json")
        rows["operations"]["snapshot_after_open"] = cli(env, name, "read", "--input", "{}", "gofin.snapshot", "--json")
        assert_liquidity_snapshot(
            rows["operations"]["snapshot_after_open"], action="Open",
            position_address=position, minimum_events=1, require_two_sided_backing=True,
        )

        add_a, add_b = token_caps(pool, wsol_atoms="500000000", usdc_atoms="50000000")[2:]
        add_request = {
            "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
            "family": family, "pool": pool_address, "position": position,
            "token_a_mint": a_mint, "token_b_mint": b_mint,
            "token_a_atoms": add_a, "token_b_atoms": add_b,
            "maximum_slippage_bps": 100, "maximum_network_fee_lamports": 20000,
            "maximum_account_funding_lamports": 10000000,
        }
        if family == "dlmm":
            add_request.update({
                "bin_lower": open_request["bin_lower"], "bin_upper": open_request["bin_upper"],
                "liquidity_distribution": open_request["liquidity_distribution"],
            })
        rows["operations"]["add"] = execute(env, name, "meteora.position.add", add_request, f"{family}-add")
        rows["operations"]["position_after_add"] = cli(env, name, "read", "--input", json.dumps({"family": family, "position": position}), "meteora.position", "--json")
        rows["operations"]["snapshot_after_add"] = cli(env, name, "read", "--input", "{}", "gofin.snapshot", "--json")
        assert_liquidity_snapshot(rows["operations"]["snapshot_after_add"], action="Add", position_address=position, minimum_events=2)

        swap_quote = quote(env, name, "swap", swap)
        swap["minimum_output_atoms"] = swap_quote["recommended_floors"]["minimum_output_atoms"]
        rows["operations"]["fee_seed_quote"] = swap_quote
        rows["operations"]["fee_seed_swap"] = execute(env, name, "meteora.swap", swap, f"{family}-fee-seed")
        reverse_swap = {**swap, "input_mint": "native", "output_mint": USDC, "amount_atoms": "100000000"}
        reverse_swap.pop("minimum_output_atoms")
        reverse_quote = quote(env, name, "swap", reverse_swap)
        reverse_swap["minimum_output_atoms"] = reverse_quote["recommended_floors"]["minimum_output_atoms"]
        rows["operations"]["reverse_swap_quote"] = reverse_quote
        rows["operations"]["reverse_swap"] = execute(env, name, "meteora.swap", reverse_swap, f"{family}-reverse")

        collect_request = {
            "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
            "family": family, "pool": pool_address, "position": position,
            "token_a_mint": a_mint, "token_b_mint": b_mint,
            "maximum_slippage_bps": 100, "maximum_network_fee_lamports": 20000,
            "maximum_account_funding_lamports": 0,
        }
        collect_quote = quote(env, name, "collect", collect_request)
        rows["operations"]["collect_quote"] = collect_quote
        rows["operations"]["collect"] = execute(env, name, "meteora.position.collect", collect_request, f"{family}-collect")
        if not positive_usable_credits(rows["operations"]["collect"]):
            raise AssertionError("Collect settled without a positive classified wallet credit")
        if not positive_collected_claims(rows["operations"]["collect"]):
            raise AssertionError("Collect settled without a positive finalized fee or reward classification")
        rows["operations"]["snapshot_after_collect"] = cli(env, name, "read", "--input", "{}", "gofin.snapshot", "--json")
        assert_liquidity_snapshot(rows["operations"]["snapshot_after_collect"], action="Collect", position_address=position, minimum_events=3)
        rows["operations"]["zero_collect_quote"] = quote(env, name, "collect", collect_request)
        assert_zero_collect_quote(rows["operations"]["zero_collect_quote"])

        for share in (5000, 10000):
            remove_request = {
                "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
                "family": family, "pool": pool_address, "position": position,
                "token_a_mint": a_mint, "token_b_mint": b_mint,
                "remove_share_bps": share,
                "maximum_slippage_bps": 100, "maximum_network_fee_lamports": 20000,
                "maximum_account_funding_lamports": 0,
            }
            remove_quote = quote(env, name, "remove", remove_request)
            floors = remove_quote.get("recommended_floors", {})
            remove_request["minimum_token_a_atoms"] = floors["minimum_token_a_atoms"]
            remove_request["minimum_token_b_atoms"] = floors["minimum_token_b_atoms"]
            label = "remove-half" if share == 5000 else "remove-full"
            rows["operations"][f"{label}_quote"] = remove_quote
            rows["operations"][label] = execute(env, name, "meteora.position.remove", remove_request, f"{family}-{label}")
            rows["operations"][f"position_after_{label}"] = cli(env, name, "read", "--input", json.dumps({"family": family, "position": position}), "meteora.position", "--json")
            rows["operations"][f"snapshot_after_{label}"] = cli(env, name, "read", "--input", "{}", "gofin.snapshot", "--json")
            assert_liquidity_snapshot(rows["operations"][f"snapshot_after_{label}"], action=label, position_address=position, minimum_events=4 if share == 5000 else 5)

        close_request = {
            "wallet": WALLET_ALIAS, "network": "solana", "cluster": "mainnet-beta",
            "family": family, "pool": pool_address, "position": position,
            "token_a_mint": a_mint, "token_b_mint": b_mint,
            "minimum_token_a_atoms": "0", "minimum_token_b_atoms": "0",
            "maximum_slippage_bps": 100, "maximum_network_fee_lamports": 20000,
            "maximum_account_funding_lamports": 0,
        }
        rows["operations"]["close_quote"] = quote(env, name, "close", close_request)
        rows["operations"]["close"] = execute(env, name, "meteora.position.close", close_request, f"{family}-close")
        if rent_change(rows["operations"]["close"]) >= 0:
            raise AssertionError("Close did not settle a refundable rent return")
        rows["operations"]["positions_after_close"] = cli(env, name, "read", "--input", json.dumps({"wallet": WALLET_ALIAS, "family": family}), "meteora.positions", "--json")
        if not rows["operations"]["positions_after_close"].get("complete") or rows["operations"]["positions_after_close"].get("positions") != []:
            raise AssertionError("Close did not leave a complete empty Meteora position set")
        rows["operations"]["snapshot_after_close"] = cli(env, name, "read", "--input", "{}", "gofin.snapshot", "--json")
        closed_state = rows["operations"]["snapshot_after_close"].get("state") or {}
        if any(candidate.get("kind") == "liquidity" and candidate.get("anchor", {}).get("evidence", {}).get("liquidity_position", {}).get("position") == position for candidate in closed_state.get("positions", [])):
            raise AssertionError("Close snapshot retained the closed LP unit anchor")
        rows["validator_transaction_count"] = command_count(helper.rpc, rpc_url, identities["wallet"]["pubkey"])
        return rows
    finally:
        if started:
            try:
                stop_environment(env, name)
            except Exception:
                pass
        node.__exit__(None, None, None)


def choose_port() -> int:
    for _ in range(512):
        port = random.randrange(20000, 45001)
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", port))
            except OSError:
                continue
        if port + 10102 <= 65535:
            return port
    raise RuntimeError("Could not find a free loopback port for the local validator")


def validate_surfpool() -> None:
    surfpool_path = Path(shutil.which("surfpool")).resolve()
    build_record = surfpool_path.parent / "surfpool-build.txt"
    patch_hash = hashlib.sha256((ROOT / "scripts/e2e/surfpool-v1.5.0.patch").read_bytes()).hexdigest()
    installer_hash = hashlib.sha256((ROOT / "scripts/e2e/install-surfpool.py").read_bytes()).hexdigest()
    binary_hash = hashlib.sha256(surfpool_path.read_bytes()).hexdigest()
    expected_build = (f"commit=86493c4bf716b01d4bedbc531aa9288cb14d5f36\npatch_sha256={patch_hash}\n"
                      f"installer_sha256={installer_hash}\nbinary_sha256={binary_hash}\nversion=surfpool 1.5.0\n")
    if (not build_record.is_file() or build_record.read_text() != expected_build
            or subprocess.check_output([str(surfpool_path), "--version"], text=True).strip() != "surfpool 1.5.0"):
        raise SystemExit("Surfpool must be built with scripts/e2e/install-surfpool.py and the current pinned patch.")

def main() -> int:
    parser = __import__("argparse").ArgumentParser(description=__doc__)
    parser.add_argument("--family", choices=("both", *FAMILY_POOLS), default="both")
    parser.add_argument("--port", type=int, help="start at this free loopback RPC port")
    args = parser.parse_args()
    for binary in ("surfpool", "cargo", "make"):
        if shutil.which(binary) is None:
            raise SystemExit(f"Required executable is missing: {binary}")
    validate_surfpool()
    helper = load_validator_helpers()
    families = list(FAMILY_POOLS) if args.family == "both" else [args.family]
    port = args.port or choose_port()
    output = {"evidence_class": "public_cli_broker_engine_local_validator", "live_write": False, "families": {}}
    path = CLI_EVIDENCE / f"public-lifecycle-{uuid.uuid4().hex}.json"
    private_directory(path.parent)
    for index, family in enumerate(families):
        selected_port = port + index * 200
        if selected_port + 10102 > 65535:
            raise SystemExit("Selected validator ports exceed the local validator port range")
        try:
            output["families"][family] = run_family(helper, family, selected_port)
        except Exception as error:
            if ACTIVE_EVIDENCE is not None:
                output["families"][family] = ACTIVE_EVIDENCE
            output["failure"] = {"family": family, "message": str(error)}
            path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
            raise RuntimeError(f"Public lifecycle failed; partial evidence is preserved at {path.relative_to(ROOT)}: {error}") from error
    path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"result": "passed", "evidence": str(path.relative_to(ROOT)), "families": list(output["families"])}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
