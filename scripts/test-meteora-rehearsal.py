#!/usr/bin/env python3
"""Exercise Meteora through the public GoFin CLI with an isolated RPC fixture."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parent.parent
DEV = ROOT / "scripts/gofin-dev.py"
SOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
POOLS = {
    "dlmm": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
    "damm_v2": "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie",
}


def run(args: list[str], env: dict[str, str], *, ok: bool = True) -> dict:
    result = subprocess.run(args, cwd=ROOT, env=env, capture_output=True, text=True, timeout=120)
    if ok and result.returncode:
        raise RuntimeError(f"{' '.join(args)} failed: {result.stderr or result.stdout}")
    if not ok:
        return {"exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"CLI did not return JSON: {result.stdout!r} {result.stderr!r}") from error


def cli(env: dict[str, str], *args: str, ok: bool = True) -> dict:
    return run([sys.executable, str(DEV), "cli", "meteora-rehearsal", "--", *args], env, ok=ok)


def reconcile_uncertain_operation(env: dict[str, str], result: dict, request_id: str) -> dict:
    """Restart and reconcile an accepted response-loss operation by its ID."""
    error = result.get("error") or {}
    if result.get("status") != "unknown" or error.get("code") != "SOLANA_SUBMISSION_UNCERTAIN":
        raise AssertionError(f"Expected the response-loss operation to be uncertain: {json.dumps(result, sort_keys=True)}")
    ledger_path = Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"])
    before = json.loads(ledger_path.read_text())
    before_count = len(before.get("transactions", []))
    signature = result.get("submission", {}).get("data", {}).get("chain_signature")
    if before_count == 0 or not signature:
        raise AssertionError("Response-loss operation omitted its accepted fixture transaction identity")
    stopped = run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)
    started = run([sys.executable, str(DEV), "--json", "start", "meteora-rehearsal", "--mode", "synthetic", "--fixture", "solana"], env)
    records_payload = cli(env, "operations", "--json")
    records = records_payload.get("operations", []) if isinstance(records_payload, dict) else records_payload
    operation = next((row for row in records if row.get("request_id") == request_id), None)
    if operation is None or not operation.get("id"):
        raise AssertionError(f"Public operations omitted uncertain request {request_id}: {json.dumps(records_payload, sort_keys=True)}")
    recovered = cli(env, "status", str(operation["id"]), "--json")
    after = json.loads(ledger_path.read_text())
    after_count = len(after.get("transactions", []))
    recovered_signature = recovered.get("submission", {}).get("data", {}).get("chain_signature")
    if (
        recovered.get("status") != "completed"
        or recovered_signature != signature
        or after_count != before_count
    ):
        raise AssertionError(
            "Public status did not reconcile the accepted response-loss transaction without replay: "
            f"status={recovered.get('status')}, signature_same={recovered_signature == signature}, "
            f"transactions_before={before_count}, transactions_after={after_count}, "
            f"response={json.dumps(recovered, sort_keys=True)}"
        )
    recovered["response_loss_recovery"] = {
        "stop": stopped,
        "restart": started,
        "request_id": request_id,
        "transactions_before": before_count,
        "transactions_after": after_count,
        "new_dispatches": after_count - before_count,
    }
    return recovered


def resume_native_submission(state: Path) -> dict:
    env = dict(os.environ)
    env.update({
        "GOFIN_DEV_STATE_HOME": str(state),
        "GOFIN_DEV_SOLANA_FIXTURE_STATE": str(state / "meteora-ledger.json"),
        "GOFIN_DEV_METEORA_REHEARSAL": "1",
        "GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_ATOMS": "1000000",
        "GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_MINT": USDC,
        "GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_OUTPUT_MINT": SOL,
    })
    ledger_path = Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"])
    if not ledger_path.is_file():
        raise RuntimeError(f"No preserved synthetic ledger exists at {ledger_path}")
    before = json.loads(ledger_path.read_text())
    transactions = before.get("transactions", [])
    if len(transactions) == 4 and before.get("lost_router_response_consumed"):
        plan = run_future_funded_named_plan(env, state.parent, before["wallet"])
        evidence = {
            "evidence_class": "public_cli_synthetic_restart_reconciliation",
            "live_network": False,
            "preserved_state": str(state),
            "standalone_transactions": len(transactions),
            "named_plan": plan,
        }
        evidence_path = state.parent / "future-funded-plan-evidence.json"
        evidence_path.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
        return evidence
    if len(transactions) == 1 and before.get("lost_router_response_consumed"):
        return continue_preserved_rehearsal(state, env, before)
    if len(transactions) < 3:
        raise RuntimeError("Preserved ledger has no previously accepted native route transaction to reconcile")
    last = transactions[-1]
    pre_usdc = next((row for row in last.get("pre_token_balances", []) if row.get("mint") == USDC), None)
    post_usdc = next((row for row in last.get("post_token_balances", []) if row.get("mint") == USDC), None)
    if not pre_usdc or not post_usdc or int(post_usdc["uiTokenAmount"]["amount"]) <= int(pre_usdc["uiTokenAmount"]["amount"]):
        raise RuntimeError("Last preserved transaction does not prove an accepted native-to-USDC swap")

    started = False
    try:
        started_result = run([sys.executable, str(DEV), "--json", "start", "meteora-rehearsal", "--mode", "synthetic", "--fixture", "solana"], env)
        started = True
        request = {
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "dlmm", "pool": POOLS["dlmm"], "input_mint": "native",
            "output_mint": USDC, "amount_atoms": "10000000", "minimum_output_atoms": "9950",
            "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
            "maximum_account_funding_lamports": 2100000,
        }
        recovered = cli(
            env,
            "execute", "meteora.swap", "--input", json.dumps(request),
            "--id", "meteora-native-route-in", "--json",
        )
        after = json.loads(ledger_path.read_text())
        before_count = len(transactions)
        after_count = len(after.get("transactions", []))
        if recovered.get("status") != "completed" or after_count != before_count:
            raise AssertionError(
                "Restart recovery did not settle the original native submission without another send: "
                f"status={recovered.get('status')}, transactions_before={before_count}, "
                f"transactions_after={after_count}, response={json.dumps(recovered, sort_keys=True)}"
            )
        return {
            "evidence_class": "public_cli_synthetic_restart_reconciliation",
            "live_network": False,
            "start": started_result,
            "recovered_operation": recovered,
            "fixture_transactions_before": before_count,
            "fixture_transactions_after": after_count,
            "new_dispatches": after_count - before_count,
        }
    finally:
        if started:
            run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)


def continue_preserved_rehearsal(state: Path, env: dict[str, str], before: dict) -> dict:
    """Continue a run whose first accepted DLMM swap lost its RPC response."""
    ledger_path = Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"])
    original_count = len(before.get("transactions", []))
    started = False
    evidence: dict[str, object] = {
        "evidence_class": "public_cli_synthetic_restart_reconciliation",
        "live_network": False,
        "preserved_state": str(state),
    }
    try:
        start = run([sys.executable, str(DEV), "--json", "start", "meteora-rehearsal", "--mode", "synthetic", "--fixture", "solana"], env)
        started = True
        records_payload = cli(env, "operations", "--json")
        records = records_payload.get("operations", []) if isinstance(records_payload, dict) else records_payload
        operation = next((row for row in records if row.get("request_id") == "meteora-dlmm-swap"), None)
        if operation is None:
            raise AssertionError("Preserved operations did not contain the original uncertain DLMM swap")
        recovered = cli(env, "status", str(operation["id"]), "--json")
        ledger = json.loads(ledger_path.read_text())
        if recovered.get("status") != "completed" or len(ledger.get("transactions", [])) != original_count:
            raise AssertionError(
                "The original response-loss operation did not reconcile without a new dispatch: "
                f"status={recovered.get('status')}, before={original_count}, after={len(ledger.get('transactions', []))}"
            )
        evidence["response_loss_recovery"] = {
            "operation": recovered,
            "transactions_before": original_count,
            "transactions_after": len(ledger["transactions"]),
            "new_dispatches": len(ledger["transactions"]) - original_count,
        }
        balance = cli(
            env, "read", "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances", "--json",
        )
        families = {}
        for family in ("damm_v2",):
            pool = POOLS[family]
            request = {
                "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
                "family": family, "pool": pool, "input_mint": USDC, "output_mint": SOL,
                "amount_atoms": "1000000", "maximum_slippage_bps": 50,
                "maximum_network_fee_lamports": 10000, "maximum_account_funding_lamports": 0,
            }
            quote = cli(env, "read", "--input", json.dumps({"action": "swap", **request}), "meteora.quote", "--json")
            request["minimum_output_atoms"] = quote["recommended_floors"]["minimum_output_atoms"]
            result = cli(env, "execute", "meteora.swap", "--input", json.dumps(request), "--id", "meteora-damm_v2-swap", "--json")
            if result.get("status") != "completed":
                raise AssertionError(f"DAMM return swap did not settle after original recovery: {json.dumps(result, sort_keys=True)}")
            families[family] = {"quote": quote, "swap": result}

        native_input = {
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "dlmm", "pool": POOLS["dlmm"], "input_mint": "native", "output_mint": USDC,
            "amount_atoms": "10000000", "maximum_slippage_bps": 50,
            "maximum_network_fee_lamports": 10000, "maximum_account_funding_lamports": 2100000,
        }
        input_quote = cli(env, "read", "--input", json.dumps({"action": "swap", **native_input}), "meteora.quote", "--json")
        native_input["minimum_output_atoms"] = input_quote["recommended_floors"]["minimum_output_atoms"]
        first_hop = cli(env, "execute", "meteora.swap", "--input", json.dumps(native_input), "--id", "meteora-native-route-in", "--json")
        if first_hop.get("status") == "unknown":
            first_hop = reconcile_uncertain_operation(env, first_hop, "meteora-native-route-in")
        if first_hop.get("status") != "completed":
            raise AssertionError(f"Native DLMM input did not settle after recovery: {json.dumps(first_hop, sort_keys=True)}")
        after_first = json.loads(ledger_path.read_text())
        tx = after_first["transactions"][-1]
        pre = next(row for row in tx["pre_token_balances"] if row["mint"] == USDC)
        post = next(row for row in tx["post_token_balances"] if row["mint"] == USDC)
        credit = int(post["uiTokenAmount"]["amount"]) - int(pre["uiTokenAmount"]["amount"])
        if credit <= 0:
            raise AssertionError("Native DLMM input did not prove a positive finalized USDC credit")
        native_output = {
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "damm_v2", "pool": POOLS["damm_v2"], "input_mint": USDC,
            "output_mint": "native", "amount_atoms": str(credit), "maximum_slippage_bps": 50,
            "maximum_network_fee_lamports": 10000, "maximum_account_funding_lamports": 2100000,
        }
        output_quote = cli(env, "read", "--input", json.dumps({"action": "swap", **native_output}), "meteora.quote", "--json")
        native_output["minimum_output_atoms"] = output_quote["recommended_floors"]["minimum_output_atoms"]
        second_hop = cli(env, "execute", "meteora.swap", "--input", json.dumps(native_output), "--id", "meteora-native-route-out", "--json")
        if second_hop.get("status") == "unknown":
            second_hop = reconcile_uncertain_operation(env, second_hop, "meteora-native-route-out")
        if second_hop.get("status") != "completed":
            raise AssertionError(f"Native DAMM output did not settle: {json.dumps(second_hop, sort_keys=True)}")
        ledger = json.loads(ledger_path.read_text())
        if len(ledger.get("transactions", [])) != 4:
            raise AssertionError("Preserved workflow did not finish with exactly four standalone swaps")
        evidence.update({"build": start, "wallet": balance, "families": families,
                         "native_route": {"input_quote": input_quote, "input_swap": first_hop,
                                          "output_quote": output_quote, "output_swap": second_hop},
                         "standalone_transactions": len(ledger["transactions"])})
        run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)
        started = False
        evidence["named_plan"] = run_future_funded_named_plan(env, state.parent, balance["wallet"])
        return evidence
    finally:
        if started:
            run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)


def exact_meteora_swap_plan(wallet_address: str, first: dict, second: dict) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"

    def asset(mint: str) -> dict:
        return {
            "network": "solana:mainnet-beta",
            "reference": "native" if mint == "native" else mint,
            "decimals": 9 if mint == "native" else 6,
        }

    def step(step_id: str, request: dict, dependencies: list[dict]) -> dict:
        return {
            "step_id": step_id,
            "operation": "meteora.swap",
            "input": {"action": "swap", **request},
            "permitted_variations": {
                "maximum_input_atoms": request["amount_atoms"],
                "minimum_output_atoms": request["minimum_output_atoms"],
                "maximum_slippage_bps": request["maximum_slippage_bps"],
                "maximum_network_fee_lamports": request["maximum_network_fee_lamports"],
                "maximum_account_funding_lamports": request["maximum_account_funding_lamports"],
            },
            "completion": {"operation_succeeded": True},
            "funding_dependencies": dependencies,
        }

    second_amount = int(second["amount_atoms"])
    native_maximum = 0
    for request in (first, second):
        if request["input_mint"] == "native":
            native_maximum += int(request["amount_atoms"])
        native_maximum += int(request["maximum_account_funding_lamports"])
    costs = sum(
        int(request["maximum_network_fee_lamports"])
        for request in (first, second)
    )
    return json.dumps(
        {
            "version": 1,
            "name": "Synthetic Meteora native round trip",
            "steps": [
                step("native-to-usdc", first, []),
                step(
                    "usdc-to-native",
                    second,
                    [{
                        "producing_step_id": "native-to-usdc",
                        "resource": resource,
                        "asset": asset(USDC),
                        "funding_scope": "principal_only",
                    }],
                ),
            ],
            "aggregate_bounds": {
                "maximum_debits": [
                    {
                        "resource": resource,
                        "amount": {"asset": asset("native"), "atoms": str(native_maximum)},
                    },
                    {
                        "resource": resource,
                        "amount": {"asset": asset(USDC), "atoms": str(second_amount)},
                    },
                ],
                "maximum_costs": [{
                    "location": {"kind": "cash", "resource": resource},
                    "amount": {"asset": asset("native"), "atoms": str(costs)},
                    "treatment": "additional_debit",
                }],
            },
        },
        separators=(",", ":"),
    )


def base58_bytes(value: str) -> bytes:
    alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    number = 0
    for char in value:
        number = number * 58 + alphabet.index(char)
    encoded = number.to_bytes((number.bit_length() + 7) // 8, "big") if number else b""
    return b"\0" * (len(value) - len(value.lstrip("1"))) + encoded


def fixture_token_atoms(ledger: dict, wallet: str, mint: str) -> int:
    wallet_bytes = base58_bytes(wallet)
    mint_bytes = base58_bytes(mint)
    matches = []
    for address, row in ledger.get("account_data", {}).items():
        if not isinstance(row, list) or len(row) != 2 or not str(row[0]).startswith("Tokenkeg"):
            continue
        data = bytes(row[1])
        if len(data) >= 72 and data[:32] == mint_bytes and data[32:64] == wallet_bytes:
            matches.append(int.from_bytes(data[64:72], "little"))
    if len(matches) != 1:
        raise AssertionError(f"Fixture expected one wallet token account for {mint}; found {len(matches)}")
    return matches[0]


def run_future_funded_named_plan(base_env: dict[str, str], state_root: Path, wallet: str) -> dict:
    """Run a cross-family plan from a fresh wallet with no USDC balance."""
    state = state_root / f"future-funded-state-{uuid.uuid4().hex[:8]}"
    env = dict(base_env)
    env.update({
        "GOFIN_DEV_STATE_HOME": str(state),
        "GOFIN_DEV_SOLANA_FIXTURE_STATE": str(state / "meteora-ledger.json"),
        "GOFIN_DEV_METEORA_REHEARSAL_INITIAL_USDC_ATOMS": "0",
    })
    started = False
    try:
        start = run([sys.executable, str(DEV), "--json", "start", "meteora-rehearsal", "--mode", "synthetic", "--fixture", "solana"], env)
        started = True
        balance = cli(
            env, "read", "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances", "--json",
        )
        if balance.get("wallet") != wallet:
            raise AssertionError("Future-funded fixture did not preserve the synthetic enrolled wallet identity")
        ledger_path = Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"])
        initial = json.loads(ledger_path.read_text())
        if initial.get("transactions") or fixture_token_atoms(initial, wallet, USDC) != 0:
            raise AssertionError("Future-funded plan fixture did not begin with exactly zero USDC and no transactions")

        first = {
            "action": "swap",
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "dlmm", "pool": POOLS["dlmm"], "input_mint": "native",
            "output_mint": USDC, "amount_atoms": "10000000",
            "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
            "maximum_account_funding_lamports": 2100000,
        }
        first_quote = cli(
            env, "read", "--input", json.dumps({"action": "swap", **first}), "meteora.quote", "--json"
        )
        first_credit = first_quote.get("quote", {}).get("token_b_credit_atoms")
        if not first_credit or int(first_credit) <= 0:
            raise AssertionError(f"Future-funded first quote omitted its positive USDC credit: {json.dumps(first_quote, sort_keys=True)}")
        first["minimum_output_atoms"] = first_quote["recommended_floors"]["minimum_output_atoms"]
        second = {
            "action": "swap",
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "damm_v2", "pool": POOLS["damm_v2"], "input_mint": USDC,
            "output_mint": "native", "amount_atoms": str(first_credit),
            "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
            "maximum_account_funding_lamports": 2100000,
        }
        # The second step is intentionally not quoted before its predecessor
        # funds the wallet. The deterministic synthetic pool converts USDC to
        # native output at 1,000 output atoms per input atom; bind a 50 bps
        # floor from that fixture contract, then obtain the actual public quote
        # after the first step's finalized credit is present.
        second["minimum_output_atoms"] = str(
            int(first_credit) * 1000 * (10_000 - second["maximum_slippage_bps"]) // 10_000
        )
        plan_input = exact_meteora_swap_plan(wallet, first, second)
        preview = cli(env, "plans", "preview", "--input", plan_input, "--valid-for", "10m", "--json")
        if not preview.get("review_hash"):
            raise AssertionError(f"Future-funded plan preview omitted its review hash: {json.dumps(preview, sort_keys=True)}")
        approval = cli(
            env, "plans", "authorize", "--input", plan_input,
            "--id", "meteora-future-funded-plan",
            "--review-hash", preview["review_hash"], "--valid-for", "10m", "--json",
        )
        plan_id = approval.get("plan_id") or approval.get("id")
        if not plan_id:
            raise AssertionError(f"Future-funded plan approval omitted its durable ID: {json.dumps(approval, sort_keys=True)}")

        before = json.loads(ledger_path.read_text())
        premature = cli(
            env, "execute", "meteora.swap", "--input", json.dumps(second),
            "--id", "meteora-future-plan-premature-usdc-to-native", "--plan", str(plan_id),
            "--step", "usdc-to-native", "--json", ok=False,
        )
        after = json.loads(ledger_path.read_text())
        try:
            premature_result = json.loads(premature["stdout"])
        except json.JSONDecodeError as error:
            raise AssertionError(f"Premature future-funded step rejection was not structured JSON: {premature!r}") from error
        if premature_result.get("status") in {"completed", "submitted"} or len(after.get("transactions", [])) != len(before.get("transactions", [])):
            raise AssertionError(f"Plan allowed its USDC-funded step before the predecessor: {json.dumps(premature_result, sort_keys=True)}")

        first_id = "meteora-future-plan-native-to-usdc"
        first_result = cli(
            env, "execute", "meteora.swap", "--input", json.dumps(first),
            "--id", first_id, "--plan", str(plan_id), "--step", "native-to-usdc", "--json",
        )
        if first_result.get("status") == "unknown":
            first_result = reconcile_uncertain_operation(env, first_result, first_id)
        after_first = json.loads(ledger_path.read_text())
        if first_result.get("status") != "completed" or not (first_result.get("effects") or {}).get("proof_digest"):
            raise AssertionError(f"Future-funded predecessor did not settle: {json.dumps(first_result, sort_keys=True)}")
        if fixture_token_atoms(after_first, wallet, USDC) <= 0 or len(after_first.get("transactions", [])) != 1:
            raise AssertionError("First plan step did not create exactly one finalized USDC-funded transaction")
        second_quote = cli(
            env, "read", "--input", json.dumps({"action": "swap", **second}), "meteora.quote", "--json"
        )
        quoted_floor = int(second_quote["recommended_floors"]["minimum_output_atoms"])
        if quoted_floor < int(second["minimum_output_atoms"]):
            raise AssertionError("Post-predecessor DAMM quote fell below the plan's fixture-derived receipt floor")
        tx = after_first["transactions"][0]
        pre_usdc = next((row for row in tx.get("pre_token_balances", []) if row.get("mint") == USDC), None)
        post_usdc = next((row for row in tx.get("post_token_balances", []) if row.get("mint") == USDC), None)
        if not pre_usdc or not post_usdc or int(pre_usdc["uiTokenAmount"]["amount"]) != 0:
            raise AssertionError("Finalized predecessor did not prove the plan began with zero USDC")

        stop = run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)
        started = False
        restart = run([sys.executable, str(DEV), "--json", "start", "meteora-rehearsal", "--mode", "synthetic", "--fixture", "solana"], env)
        started = True
        duplicate_first = cli(
            env, "execute", "meteora.swap", "--input", json.dumps(first),
            "--id", first_id, "--plan", str(plan_id), "--step", "native-to-usdc", "--json",
        )
        after_duplicate = json.loads(ledger_path.read_text())
        if duplicate_first.get("status") != "completed" or len(after_duplicate.get("transactions", [])) != 1:
            raise AssertionError("Restart recovery replayed or lost the completed named-plan predecessor")
        second_result = cli(
            env, "execute", "meteora.swap", "--input", json.dumps(second),
            "--id", "meteora-future-plan-usdc-to-native", "--plan", str(plan_id),
            "--step", "usdc-to-native", "--json",
        )
        ledger = json.loads(ledger_path.read_text())
        if (
            second_result.get("status") != "completed"
            or not (second_result.get("effects") or {}).get("proof_digest")
            or len(ledger.get("transactions", [])) != 2
        ):
            raise AssertionError(f"Future-funded second step did not settle once after restart: {json.dumps(second_result, sort_keys=True)}")
        second_tx = ledger["transactions"][1]
        second_before = next((row for row in second_tx.get("pre_token_balances", []) if row.get("mint") == USDC), None)
        second_after = next((row for row in second_tx.get("post_token_balances", []) if row.get("mint") == USDC), None)
        if not second_before or not second_after:
            raise AssertionError("Second finalized proof omitted its USDC debit")
        before_atoms = int(second_before["uiTokenAmount"]["amount"])
        after_atoms = int(second_after["uiTokenAmount"]["amount"])
        if before_atoms <= 0 or before_atoms - after_atoms != int(second["amount_atoms"]):
            raise AssertionError("Second plan step did not consume exactly its predecessor-funded USDC amount")
        status = cli(env, "plans", "status", str(plan_id), "--json")
        plan_state = status.get("state", status.get("status"))
        steps = status.get("steps", [])
        expected_steps = {"native-to-usdc", "usdc-to-native"}
        plan_completed = (
            {step.get("step_id") for step in steps} == expected_steps
            and all(step.get("state") == "completed" for step in steps)
            and plan_state in (None, "completed")
        )
        if not plan_completed:
            raise AssertionError(f"Future-funded plan remained incomplete after both proofs: {json.dumps(status, sort_keys=True)}")
        return {
            "evidence_class": "public_cli_synthetic_future_funded_plan",
            "initial_usdc_atoms": "0",
            "premature_step_rejection": premature_result,
            "first_quote": first_quote,
            "second_quote": second_quote,
            "approval": approval,
            "first_step": first_result,
            "duplicate_after_restart": duplicate_first,
            "second_step": second_result,
            "status": status,
            "transactions": len(ledger["transactions"]),
            "predecessor_credit_atoms": str(int(post_usdc["uiTokenAmount"]["amount"])),
            "second_step_debit_atoms": str(before_atoms - after_atoms),
            "stop": stop,
            "restart": restart,
        }
    finally:
        if started:
            run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)
def main() -> int:
    if len(sys.argv) == 3 and sys.argv[1] == "--resume-state":
        print(json.dumps(resume_native_submission(Path(sys.argv[2]).resolve()), indent=2, sort_keys=True))
        return 0
    if len(sys.argv) != 1:
        raise RuntimeError("Usage: test-meteora-rehearsal.py [--resume-state STATE_HOME]")
    evidence: dict[str, object] = {"evidence_class": "public_cli_synthetic", "live_network": False, "families": {}}
    state_root = ROOT / ".context" / "meteora-rehearsal-runs" / uuid.uuid4().hex
    state = state_root / "state"
    env = dict(os.environ)
    env.update({
        "GOFIN_DEV_STATE_HOME": str(state),
        "GOFIN_DEV_SOLANA_FIXTURE_STATE": str(state / "meteora-ledger.json"),
        "GOFIN_DEV_METEORA_REHEARSAL": "1",
        # Exercise accepted-transaction/lost-send-response reconciliation
        # on the first DLMM write using the existing WSOL ATA; the fixture
        # matches the exact protocol input/output token identities.
        "GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_ATOMS": "1000000",
        "GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_MINT": USDC,
        "GOFIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_OUTPUT_MINT": SOL,
    })
    started = False
    failed = False
    try:
        start = run([sys.executable, str(DEV), "--json", "start", "meteora-rehearsal", "--mode", "synthetic", "--fixture", "solana"], env)
        started = True
        evidence["build"] = start
        balance = cli(
            env,
            "read",
            "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances",
            "--json",
        )
        if balance.get("observed_commitment") != "finalized" or not balance.get("wallet"):
            raise AssertionError(f"Synthetic wallet read omitted its finalized identity: {json.dumps(balance, sort_keys=True)}")
        families: dict[str, object] = {}
        for family, pool in POOLS.items():
            request = {"family": family, "pool": pool}
            inspected = cli(env, "read", "--input", json.dumps(request), "meteora.pool", "--json")
            pool_info = inspected["pool"]
            if pool_info["token_a"]["mint"] != SOL or pool_info["token_b"]["mint"] != USDC:
                raise AssertionError(f"{family} pool fixture returned unexpected mint identities")
            swap = {
                "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
                "family": family, "pool": pool, "input_mint": USDC, "output_mint": SOL,
                "amount_atoms": "1000000", "minimum_output_atoms": "995000000",
                "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
                "maximum_account_funding_lamports": 0,
            }
            quote_input = {"action": "swap", **{k: v for k, v in swap.items() if k != "minimum_output_atoms"}}
            quote = cli(env, "read", "--input", json.dumps(quote_input), "meteora.quote", "--json")
            recommended = int(quote["recommended_floors"]["minimum_output_atoms"])
            if recommended != 995_000_000:
                raise AssertionError(f"{family} quote recommended an unexpected floor: {recommended}")
            ledger_before_weak_floor = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            writes_before_rejection = len(ledger_before_weak_floor.get("transactions", []))
            weak_floor = dict(swap, minimum_output_atoms="1")
            rejected = cli(env, "execute", "meteora.swap", "--input", json.dumps(weak_floor), "--id", f"meteora-{family}-weak-floor", "--json", ok=False)
            ledger_before_write = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            writes_after_rejection = len(ledger_before_write.get("transactions", []))
            if writes_after_rejection != writes_before_rejection:
                raise AssertionError(
                    f"{family} submitted despite weak output floor; response={json.dumps(rejected, sort_keys=True)}; "
                    f"transactions_before={writes_before_rejection}; transactions_after={writes_after_rejection}"
                )
            try:
                weak_result = json.loads(rejected["stdout"])
            except json.JSONDecodeError as error:
                raise AssertionError(f"{family} weak-floor response was not structured JSON: {rejected!r}") from error
            weak_error = weak_result.get("error") or {}
            if weak_result.get("status") != "failed" or weak_error.get("code") != "INVALID_INPUT" or "minimum_output_atoms" not in weak_error.get("message", ""):
                raise AssertionError(
                    f"{family} weak-floor request did not return a structured validation failure: "
                    f"{json.dumps(rejected, sort_keys=True)}; transactions_before={writes_before_rejection}; "
                    f"transactions_after={writes_after_rejection}"
                )
            rejected = {**rejected, "operation": weak_result}
            swap["minimum_output_atoms"] = str(recommended)
            result = cli(env, "execute", "meteora.swap", "--input", json.dumps(swap), "--id", f"meteora-{family}-swap", "--json")
            if result.get("status") == "unknown":
                result = reconcile_uncertain_operation(env, result, f"meteora-{family}-swap")
            if result.get("status") != "completed":
                raise AssertionError(f"{family} swap did not settle: {json.dumps(result, sort_keys=True)}")
            candidate_family = result.get("preview", {}).get("binding", {}).get("pool", {}).get("family")
            if candidate_family != family:
                raise AssertionError(f"{family} candidate was not bound to its exact protocol family: {candidate_family}")
            before_duplicate = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            transaction_count = len(before_duplicate.get("transactions", []))
            duplicate = cli(env, "execute", "meteora.swap", "--input", json.dumps(swap), "--id", f"meteora-{family}-swap", "--json")
            if duplicate.get("status") != "completed":
                raise AssertionError(f"{family} duplicate operation ID did not return its settled result")
            after_duplicate = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            if len(after_duplicate.get("transactions", [])) != transaction_count:
                raise AssertionError(f"{family} duplicate operation ID dispatched another transaction")
            families[family] = {"pool": inspected, "quote": quote, "weak_floor_rejection": rejected, "swap": result, "duplicate_operation_replay": duplicate}
        # A two-hop native route proves that the first finalized credit is
        # the exact source debit of the second write. The route traverses
        # separate protocol adapters and temporary WSOL setup/close paths.
        native_input = {
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "dlmm", "pool": POOLS["dlmm"], "input_mint": "native",
            "output_mint": USDC, "amount_atoms": "10000000",
            "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
            "maximum_account_funding_lamports": 2100000,
        }
        native_input_quote = cli(env, "read", "--input", json.dumps({"action": "swap", **native_input}), "meteora.quote", "--json")
        if (
            native_input_quote.get("quote", {}).get("native_cash_delta") != "-10000000"
            or native_input_quote.get("transaction_requirements", {}).get("expected_network_fee_lamports") != "10000"
        ):
            raise AssertionError(
                "Native input quote did not separate the two-signature 10,000-lamport network fee from the 10,000,000-lamport trade debit: "
                f"{json.dumps(native_input_quote, sort_keys=True)}"
            )
        native_input["minimum_output_atoms"] = native_input_quote["recommended_floors"]["minimum_output_atoms"]
        first_hop = cli(env, "execute", "meteora.swap", "--input", json.dumps(native_input), "--id", "meteora-native-route-in", "--json")
        if first_hop.get("status") != "completed":
            ledger = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            raise AssertionError(
                f"native SOL to USDC DLMM hop did not settle: {json.dumps(first_hop, sort_keys=True)}; "
                f"finalized_fixture_transactions={len(ledger.get('transactions', []))}"
            )
        after_first_hop = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
        if not after_first_hop.get("lost_router_response_consumed"):
            raise AssertionError("Native route did not exercise accepted-transaction response-loss recovery")
        first_transaction = after_first_hop["transactions"][-1]
        first_credit_before = next((row for row in first_transaction["pre_token_balances"] if row["mint"] == USDC), None)
        first_credit_after = next((row for row in first_transaction["post_token_balances"] if row["mint"] == USDC), None)
        if first_credit_before is None or first_credit_after is None:
            raise AssertionError("Native DLMM hop did not produce a finalized USDC credit")
        finalized_usdc_credit = str(
            int(first_credit_after["uiTokenAmount"]["amount"])
            - int(first_credit_before["uiTokenAmount"]["amount"])
        )
        if int(finalized_usdc_credit) <= 0:
            raise AssertionError("Native DLMM hop finalized a non-positive USDC credit")

        native_output = {
            "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
            "family": "damm_v2", "pool": POOLS["damm_v2"], "input_mint": USDC,
            "output_mint": "native", "amount_atoms": finalized_usdc_credit,
            "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
            "maximum_account_funding_lamports": 2100000,
        }
        native_output_quote = cli(env, "read", "--input", json.dumps({"action": "swap", **native_output}), "meteora.quote", "--json")
        if (
            native_output_quote.get("quote", {}).get("native_cash_delta") != "10000000"
            or native_output_quote.get("transaction_requirements", {}).get("expected_network_fee_lamports") != "10000"
        ):
            raise AssertionError(
                "Native output quote did not separate the two-signature 10,000-lamport network fee from the trade credit: "
                f"{json.dumps(native_output_quote, sort_keys=True)}"
            )
        native_output["minimum_output_atoms"] = native_output_quote["recommended_floors"]["minimum_output_atoms"]
        second_hop = cli(env, "execute", "meteora.swap", "--input", json.dumps(native_output), "--id", "meteora-native-route-out", "--json")
        if second_hop.get("status") != "completed":
            ledger = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            raise AssertionError(
                f"USDC to native SOL DAMM hop did not settle: {json.dumps(second_hop, sort_keys=True)}; "
                f"finalized_fixture_transactions={len(ledger.get('transactions', []))}"
            )
        evidence["native_route"] = {
            "input_quote": native_input_quote, "input_swap": first_hop,
            "output_quote": native_output_quote, "output_swap": second_hop,
        }
        evidence["families"] = families
        ledger = json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
        transactions = ledger.get("transactions", [])
        if len(transactions) != 4:
            raise AssertionError("Fixture did not record exactly four finalized swaps")
        route_credit_after = next((row for row in transactions[2]["post_token_balances"] if row["mint"] == USDC), None)
        route_debit_before = next((row for row in transactions[3]["pre_token_balances"] if row["mint"] == USDC), None)
        route_debit_after = next((row for row in transactions[3]["post_token_balances"] if row["mint"] == USDC), None)
        if not route_credit_after or not route_debit_before or not route_debit_after:
            raise AssertionError("Native route finalized proofs omitted the USDC side needed to verify the hop")
        credit_total_atoms = int(route_credit_after["uiTokenAmount"]["amount"])
        debit_before_atoms = int(route_debit_before["uiTokenAmount"]["amount"])
        debit_after_atoms = int(route_debit_after["uiTokenAmount"]["amount"])
        if debit_before_atoms != credit_total_atoms or debit_before_atoms - debit_after_atoms != int(native_output["amount_atoms"]):
            raise AssertionError("DAMM native-output debit did not consume the exact USDC balance finalized by the DLMM first hop")
        evidence["synthetic_transactions"] = len(ledger["transactions"])

        # A separate fresh fixture begins with no USDC. Its second leg is
        # rejected until the first leg's finalized credit, then survives a
        # broker restart without replaying the predecessor transaction.
        evidence["named_plan"] = run_future_funded_named_plan(env, state_root, balance["wallet"])
    except Exception:
        failed = True
        raise
    finally:
        try:
            if started:
                run([sys.executable, str(DEV), "--json", "stop", "meteora-rehearsal"], env)
        finally:
            if failed and state.is_dir():
                print(f"Preserved synthetic state for restart recovery: {state}", file=sys.stderr)

    print(json.dumps(evidence, indent=2, sort_keys=True))
    shutil.rmtree(state_root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"Meteora rehearsal failed: {error}", file=sys.stderr)
        raise SystemExit(1)
