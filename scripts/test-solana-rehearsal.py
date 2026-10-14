#!/usr/bin/env python3
"""Exercise Solana wallet and Jupiter swaps through the public CLI and broker.

All credentials, RPC, Jupiter responses, approval and SQLite state are synthetic
and isolated in a temporary development environment. No network or .env files
are used.
"""
from __future__ import annotations

import json
import argparse
import base64
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sqlite3
import sys
import tempfile
import time

from solana_trigger_lifecycle import run_partial_then_full_fill
from solana_perps_tpsl_plan import run_named_plan_tpsl
from solana_perps_limit_plan import run_named_plan_limit

REPO = Path(__file__).resolve().parent.parent
DEV = REPO / "scripts/fin-dev.py"
INPUT_MINT = "So11111111111111111111111111111111111111112"
OUTPUT_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
PYUSD_MINT = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo"
USDC_MINT = OUTPUT_MINT
JLP_MINT = "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4"
ORDER_ENGINE_PROGRAM = "61DFfeTKM7trxYcPQCM78bJ794ddZprZpAwAnLiwTpYH"
ORDER_ENGINE_MAKER = "Gymcte79ALGtFrWybRxqb7M7uRjnbjgaHjGgQf1v4qB8"
JUPITER_V6_PROGRAM = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"


def decode_cli_output(stdout: str, args: list[str]) -> dict:
    if "--stream" not in args:
        return json.loads(stdout)
    events = [json.loads(line) for line in stdout.splitlines()]
    if not events or events[-1].get("type") not in {"result", "error"}:
        raise AssertionError("Stream ended without a terminal event; recover the original operation.")
    if any(event.get("type") != "progress" for event in events[:-1]):
        raise AssertionError("Stream must contain progress followed by exactly one terminal event.")
    allowed = {"connecting", "preparing", "awaiting_approval", "revalidating", "refreshing_candidate", "submitting", "verifying"}
    if any(event.get("stage") not in allowed for event in events[:-1]):
        raise AssertionError("Unexpected execution stage in NDJSON progress.")
    if "\x1b" in stdout:
        raise AssertionError("Machine stream contains terminal control characters.")
    final = events[-1]
    return final["result"] if final["type"] == "result" else {"error": final["error"]}


def execution_args(args: list[str]) -> list[str]:
    # Exercise the new machine UX through actual CLI/broker financial workflows;
    # read/discovery calls continue checking the single-value JSON contract.
    if args and args[0] in {"execute", "status"} and "--json" in args and "--stream" not in args:
        return [*args, "--stream"]
    return args


def call(name: str, args: list[str], env: dict[str, str]) -> dict:
    args = execution_args(args)
    result = subprocess.run(
        [sys.executable, str(DEV), "cli", name, "--", *args],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    if result.returncode:
        raise RuntimeError(
            f"CLI command failed ({' '.join(args[:2])}): {result.stderr or result.stdout}"
        )
    try:
        return decode_cli_output(result.stdout, args)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"CLI did not return JSON for {' '.join(args[:2])}.") from error


def call_pending(
    name: str,
    args: list[str],
    env: dict[str, str],
    stage_events: list[str] | None = None,
) -> dict:
    """Keep structured pending/unknown output even when Fin uses a failure exit code."""
    args = execution_args(args)
    result = subprocess.run(
        [sys.executable, str(DEV), "cli", name, "--", *args],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    try:
        value = decode_cli_output(result.stdout, args)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"CLI did not return structured JSON for {' '.join(args[:2])}: "
            f"{result.stderr or result.stdout}"
        ) from error
    if not isinstance(value, dict):
        raise RuntimeError(f"CLI returned a non-object for {' '.join(args[:2])}.")
    if stage_events is not None and "--stream" in args:
        stage_events.extend(
            event.get("stage")
            for line in result.stdout.splitlines()
            if (event := json.loads(line)).get("type") == "progress"
        )
    return value


def swap_input(
    route: str,
    amount: str,
    minimum: str,
    input_mint: str = INPUT_MINT,
    output_mint: str = OUTPUT_MINT,
) -> str:
    return json.dumps(
        {
            "wallet": "solana-rehearsal",
            "network": "solana",
            "cluster": "mainnet-beta",
            "input_mint": input_mint,
            "output_mint": output_mint,
            "amount_atoms": amount,
            "minimum_output_atoms": minimum,
            "maximum_slippage_bps": 50,
            "maximum_platform_fee_bps": 0,
            "maximum_network_fee_lamports": 10_000,
            "maximum_account_funding_lamports": 3_000_000,
            "route": route,
        },
        separators=(",", ":"),
    )


def future_funded_plan(wallet_address: str) -> str:
    wallet_resource = f"solana:mainnet-beta:{wallet_address}"
    sol = {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}
    wsol = {"network": "solana:mainnet-beta", "reference": INPUT_MINT, "decimals": 9}
    return json.dumps(
        {
            "version": 1,
            "name": "Synthetic future funded wrap and swap",
            "steps": [
                {
                    "step_id": "wrap",
                    "operation": "wallet.wrap",
                    "input": {
                        "wallet": "solana-rehearsal",
                        "network": "solana",
                        "cluster": "mainnet-beta",
                        "amount": "0.02",
                        "max_fee_sol": "0.00001",
                        "max_account_funding_sol": "0.003",
                        "requestId": "solana-plan-wrap",
                    },
                    "permitted_variations": {
                        "max_amount": "0.02",
                        "max_fee_sol": "0.00001",
                        "max_account_funding_sol": "0.003",
                    },
                    "completion": {"operation_succeeded": True},
                    "funding_dependencies": [],
                },
                {
                    "step_id": "swap",
                    "operation": "jupiter.swap",
                    "input": json.loads(swap_input("router", "20000000", "1990000")),
                    "permitted_variations": {
                        "maximum_input_atoms": "20000000",
                        "minimum_output_per_input_numerator": "995",
                        "minimum_output_per_input_denominator": "10000",
                        "maximum_slippage_bps": 50,
                        "maximum_platform_fee_bps": 0,
                        "maximum_network_fee_lamports": 10000,
                        "maximum_account_funding_lamports": 3000000,
                        "maximum_quote_lifetime_seconds": 600,
                    },
                    "completion": {"operation_succeeded": True},
                    "funding_dependencies": [
                        {
                            "producing_step_id": "wrap",
                            "resource": wallet_resource,
                            "asset": wsol,
                            "funding_scope": "principal_only",
                        }
                    ],
                },
            ],
            "aggregate_bounds": {
                "maximum_debits": [
                    {"resource": wallet_resource, "amount": {"asset": sol, "atoms": "23039280"}},
                    {"resource": wallet_resource, "amount": {"asset": wsol, "atoms": "20000000"}},
                ],
                "maximum_costs": [
                    {
                        "location": {"kind": "cash", "resource": wallet_resource},
                        "amount": {"asset": sol, "atoms": "20000"},
                        "treatment": "additional_debit",
                    }
                ],
            },
        },
        separators=(",", ":"),
    )


def native_sol_roundtrip_plan(wallet_address: str) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"
    sol = {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}
    usdc = {"network": "solana:mainnet-beta", "reference": OUTPUT_MINT, "decimals": 6}

    def exact_scope(
        maximum_debits: list[dict],
        maximum_costs: list[dict],
        possible_credits: list[dict],
    ) -> dict:
        return {
            "mode": "exact",
            "maximum_debits": maximum_debits,
            "maximum_costs": maximum_costs,
            "possible_credits": possible_credits,
        }

    def debit(asset: dict, atoms: str) -> dict:
        return {"resource": resource, "amount": {"asset": asset, "atoms": atoms}}

    def cost(atoms: str) -> dict:
        return {
            "location": {"kind": "cash", "resource": resource},
            "amount": {"asset": sol, "atoms": atoms},
            "treatment": "additional_debit",
        }

    def credit(asset: dict) -> dict:
        return {"resource": resource, "asset": asset}

    return json.dumps(
        {
            "version": 1,
            "name": "Synthetic native SOL swap round trip with temporary WSOL",
            "steps": [
                {
                    "step_id": "native-in",
                    "operation": "jupiter.swap",
                    "input": json.loads(swap_input("meta_aggregator", "20000000", "1990000", "native", OUTPUT_MINT)),
                    "permitted_variations": exact_scope(
                        [debit(sol, "22039280")], [cost("10000")], [credit(usdc)]
                    ),
                    "completion": {"operation_succeeded": True},
                    "funding_dependencies": [],
                },
                {
                    "step_id": "native-out",
                    "operation": "jupiter.swap",
                    "input": json.loads(swap_input("router", "2000000", "19900000", OUTPUT_MINT, "native")),
                    "permitted_variations": exact_scope(
                        [debit(usdc, "2000000"), debit(sol, "2039280")],
                        [cost("10000")],
                        [credit(sol)],
                    ),
                    "completion": {"operation_succeeded": True},
                    "funding_dependencies": [
                        {
                            "producing_step_id": "native-in",
                            "resource": resource,
                            "asset": usdc,
                            "funding_scope": "principal_only",
                        }
                    ],
                },
            ],
            "aggregate_bounds": {
                "maximum_debits": [
                    {"resource": resource, "amount": {"asset": sol, "atoms": "24078560"}},
                    {"resource": resource, "amount": {"asset": usdc, "atoms": "2000000"}},
                ],
                "maximum_costs": [
                    {
                        "location": {"kind": "cash", "resource": resource},
                        "amount": {"asset": sol, "atoms": "20000"},
                        "treatment": "additional_debit",
                    }
                ],
            },
        },
        separators=(",", ":"),
    )


def exact_swap_plan(wallet_address: str, name: str, step_id: str, swap: dict) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"

    def asset_for(mint: str) -> dict:
        return {
            "network": "solana:mainnet-beta",
            "reference": "native" if mint == "native" else mint,
            "decimals": 9 if mint == "native" else 6,
        }

    input_asset = asset_for(swap["input_mint"])
    output_asset = asset_for(swap["output_mint"])
    debit_rows = [{
        "resource": resource,
        "amount": {"asset": input_asset, "atoms": swap["amount_atoms"]},
    }]
    if swap["input_mint"] == "native":
        debit_rows.append({
            "resource": resource,
            "amount": {"asset": asset_for("native"), "atoms": "2039280"},
        })
    costs = [{
        "location": {"kind": "cash", "resource": resource},
        "amount": {"asset": asset_for("native"), "atoms": "10000"},
        "treatment": "additional_debit",
    }]
    scope = {
        "mode": "exact",
        "maximum_debits": debit_rows,
        "maximum_costs": costs,
        "possible_credits": [{"resource": resource, "asset": output_asset}],
    }
    aggregate_debits = list(debit_rows)
    if swap["input_mint"] == "native":
        aggregate_debits = [{
            "resource": resource,
            "amount": {
                "asset": asset_for("native"),
                "atoms": str(int(swap["amount_atoms"]) + 2_039_280),
            },
        }]
        scope["maximum_debits"] = aggregate_debits
    return json.dumps({
        "version": 1,
        "name": name,
        "steps": [{
            "step_id": step_id,
            "operation": "jupiter.swap",
            "input": swap,
            "permitted_variations": scope,
            "completion": {"operation_succeeded": True},
            "funding_dependencies": [],
        }],
        "aggregate_bounds": {
            "maximum_debits": aggregate_debits,
            "maximum_costs": costs,
        },
    }, separators=(",", ":"))


def run_expiry_rehearsal(
    name: str, env: dict[str, str], mode: str, wallet_address: str, route: str
) -> dict:
    """Exercise bounded expiry refresh through the public CLI and synthetic broker."""
    swap = json.loads(swap_input(route, "1000000", "995000", OUTPUT_MINT, PYUSD_MINT))
    plan_input = exact_swap_plan(wallet_address, f"Synthetic expiry rehearsal {mode}", "swap", swap)
    preview = call(name, ["plans", "preview", "--input", plan_input, "--json"], env)
    approval = call(
        name,
        ["plans", "authorize", "--input", plan_input, "--id", f"solana-expiry-{mode}-plan",
         "--review-hash", preview["review_hash"], "--json"],
        env,
    )
    plan_id = approval.get("plan_id") or approval.get("id")
    if not plan_id:
        raise AssertionError("Expiry rehearsal authorization omitted its durable plan ID.")

    stages: list[str] = []
    result = call_pending(
        name,
        ["execute", "jupiter.swap", "--input", json.dumps(swap),
         "--id", f"solana-expiry-{mode}-attempt", "--plan", plan_id, "--step", "swap", "--json"],
        env,
        stage_events=stages,
    )
    status = call(name, ["plans", "status", plan_id, "--json"], env)
    fixture = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
    steps = status.get("steps", [])
    step = next((entry for entry in steps if entry.get("step_id") == "swap"), None)
    if step is None:
        raise AssertionError(f"Expiry rehearsal plan status omitted its swap step: {json.dumps(status, sort_keys=True)}")
    attempts = step.get("attempts", [])
    expected_candidates = {"revalidate-once": 2, "dispatch-once": 2,
                           "revalidate-every": 3, "dispatch-every": 3,
                           "uncertain": 1}[mode]
    if fixture.get("expiry_candidate_count") != expected_candidates:
        raise AssertionError(
            f"Expiry candidate count was {fixture.get('expiry_candidate_count')}, "
            f"expected {expected_candidates}; result={json.dumps(result, sort_keys=True)}"
        )
    expected_status = {
        "revalidate-once": "completed", "dispatch-once": "completed",
        "revalidate-every": "expired", "dispatch-every": "failed", "uncertain": "unknown",
    }[mode]
    if result.get("status") != expected_status:
        raise AssertionError(
            f"Expiry mode {mode} returned {result.get('status')!r}; expected {expected_status!r} "
            f"(error={(result.get('error') or {}).get('code')!r})."
        )
    if len(attempts) != expected_candidates:
        raise AssertionError(
            f"Expiry attempt linkage did not match fresh candidates ({len(attempts)} vs "
            f"{expected_candidates}): {json.dumps(step, sort_keys=True)}"
        )
    if "awaiting_approval" in stages:
        raise AssertionError("Automatic expiry refresh unexpectedly requested another approval.")
    expected_transactions = 0 if mode in {"revalidate-every", "dispatch-every"} else 1
    if len(fixture.get("transactions", [])) != expected_transactions:
        raise AssertionError(
            f"Expiry mode {mode} recorded {len(fixture.get('transactions', []))} transactions; "
            f"expected {expected_transactions}."
        )
    if mode in {"revalidate-once", "dispatch-once"} and "refreshing_candidate" not in stages:
        raise AssertionError(f"CLI stream omitted refreshing_candidate: {stages}")
    if mode in {"revalidate-every", "dispatch-every"} and stages.count("refreshing_candidate") != 2:
        raise AssertionError(f"Two-refresh exhaustion was not visible in progress: {stages}")
    if mode in {"revalidate-every", "dispatch-every"}:
        failure = result.get("error") or {}
        details = failure.get("details") or {}
        if details.get("refresh_count") != 2 or details.get("refresh_limit") != 2:
            raise AssertionError("Expiry exhaustion did not report exactly two bounded refreshes.")

    evidence: dict[str, object] = {
        "mode": mode,
        "route": route,
        "status": result.get("status"),
        "plan_id": plan_id,
        "attempt_ids": [a.get("attempt_id") or a.get("operation_id") for a in attempts],
        "stages": stages,
        "candidate_count": fixture.get("expiry_candidate_count"),
        "height_checks_last_candidate": fixture.get("expiry_candidate_height_checks"),
        "rpc_method_counts": fixture.get("rpc_method_counts", {}),
    }
    if mode == "uncertain":
        before_duplicate = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
        duplicate = call_pending(
            name,
            ["execute", "jupiter.swap", "--input", json.dumps(swap),
             "--id", f"solana-expiry-{mode}-attempt", "--plan", plan_id, "--step", "swap", "--json"],
            env,
        )
        restart_fixture(name, env)
        after_restart = call_pending(name, ["status", result.get("id"), "--json"], env)
        after_duplicate = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
        if (
            after_duplicate.get("expiry_candidate_count") != before_duplicate.get("expiry_candidate_count")
            or len(after_duplicate.get("transactions", [])) != len(before_duplicate.get("transactions", []))
            or "refreshing_candidate" in stages
        ):
            raise AssertionError("An uncertain signed transaction was refreshed or replayed.")
        evidence["uncertain"] = {
            "status": result.get("status"),
            "duplicate_status": duplicate.get("status"),
            "restart_status": after_restart.get("status"),
            "candidate_count": after_duplicate.get("expiry_candidate_count"),
            "transaction_count": len(after_duplicate.get("transactions", [])),
        }
    if mode == "dispatch-once":
        first = attempts[0]
        original_id = first.get("attempt_id") or first.get("operation_id")
        original = call_pending(name, ["status", original_id, "--json"], env)
        submission = original.get("submission") or {}
        dispatch = original.get("dispatch_evidence") or original.get("evidence", {}).get("dispatch_evidence") or {}
        if original.get("status") != "failed" or not submission:
            raise AssertionError(f"Expired signed attempt was not durably terminal: {json.dumps(original, sort_keys=True)}")
        if dispatch.get("no_network_send") is not True:
            raise AssertionError(f"Expired attempt lacks verified no-send evidence: {json.dumps(original, sort_keys=True)}")
        digest = (submission.get("data") or {}).get("message_sha256") or submission.get("message_sha256")
        if not digest or dispatch.get("message_sha256") != digest:
            raise AssertionError("No-send proof was not bound to the immutable signed message.")
        if original.get("effects", {}).get("proof_digest") is None:
            raise AssertionError("Expired signed attempt omitted adapter-verified empty settlement proof.")
        before_duplicate = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
        duplicate = call_pending(
            name,
            ["execute", "jupiter.swap", "--input", json.dumps(swap),
             "--id", f"solana-expiry-{mode}-attempt", "--plan", plan_id, "--step", "swap", "--json"],
            env,
        )
        after_duplicate = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
        if (
            after_duplicate.get("expiry_candidate_count") != before_duplicate.get("expiry_candidate_count")
            or len(after_duplicate.get("transactions", [])) != len(before_duplicate.get("transactions", []))
        ):
            raise AssertionError("Duplicate logical execution prepared or dispatched another candidate.")
        restart_fixture(name, env)
        original_after_restart = call_pending(name, ["status", original_id, "--json"], env)
        if (
            original_after_restart.get("status") != "failed"
            or original_after_restart.get("submission") != submission
        ):
            raise AssertionError("Restart changed the immutable terminal no-send attempt record.")
        evidence["expired_original"] = {
            "status": original.get("status"),
            "reason_code": dispatch.get("reason_code"),
            "no_network_send": dispatch.get("no_network_send"),
            "message_sha256": digest,
            "proof_digest": original["effects"]["proof_digest"],
            "duplicate_status": duplicate.get("status"),
            "restart_status": original_after_restart.get("status"),
        }
    if mode == "dispatch-every":
        for item in attempts:
            attempt_id = item.get("attempt_id") or item.get("operation_id")
            record = call_pending(name, ["status", attempt_id, "--json"], env)
            dispatch = record.get("dispatch_evidence") or (record.get("evidence") or {}).get("dispatch_evidence") or {}
            submission = record.get("submission") or {}
            digest = (submission.get("data") or {}).get("message_sha256") or submission.get("message_sha256")
            if (
                record.get("status") != "failed"
                or dispatch.get("no_network_send") is not True
                or dispatch.get("message_sha256") != digest
                or not (record.get("effects") or {}).get("proof_digest")
            ):
                raise AssertionError("An exhausted signed attempt lacked exact verified no-send proof.")
    return evidence


def exact_swap_roundtrip_plan(
    wallet_address: str, name: str, first: dict, second: dict
) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"

    def asset_for(mint: str) -> dict:
        return {
            "network": "solana:mainnet-beta",
            "reference": "native" if mint == "native" else mint,
            "decimals": 9 if mint == "native" else 6,
        }

    def scope(swap: dict) -> dict:
        return {
            "mode": "exact",
            "maximum_debits": [{
                "resource": resource,
                "amount": {
                    "asset": asset_for(swap["input_mint"]),
                    "atoms": swap["amount_atoms"],
                },
            }],
            "maximum_costs": [{
                "location": {"kind": "cash", "resource": resource},
                "amount": {"asset": asset_for("native"), "atoms": "10000"},
                "treatment": "additional_debit",
            }],
            "possible_credits": [{
                "resource": resource,
                "asset": asset_for(swap["output_mint"]),
            }],
        }

    first_asset = asset_for(first["output_mint"])
    return json.dumps({
        "version": 1,
        "name": name,
        "steps": [
            {
                "step_id": "convert-out",
                "operation": "jupiter.swap",
                "input": first,
                "permitted_variations": scope(first),
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            },
            {
                "step_id": "convert-back",
                "operation": "jupiter.swap",
                "input": second,
                "permitted_variations": scope(second),
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [{
                    "producing_step_id": "convert-out",
                    "resource": resource,
                    "asset": first_asset,
                    "funding_scope": "principal_only",
                }],
            },
        ],
        "aggregate_bounds": {
            "maximum_debits": [
                {"resource": resource, "amount": {"asset": asset_for(first["input_mint"]), "atoms": first["amount_atoms"]}},
                {"resource": resource, "amount": {"asset": asset_for(second["input_mint"]), "atoms": second["amount_atoms"]}},
            ],
            "maximum_costs": [{
                "location": {"kind": "cash", "resource": resource},
                "amount": {"asset": asset_for("native"), "atoms": "20000"},
                "treatment": "additional_debit",
            }],
        },
    }, separators=(",", ":"))


def prepared_binding(record: dict) -> dict:
    return (
        record.get("binding")
        or (record.get("preview") or {}).get("binding")
        or (record.get("prepared") or {}).get("binding")
        or {}
    )


def base58_bytes(value: str) -> bytes:
    alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    number = 0
    for character in value:
        number = number * 58 + alphabet.index(character)
    payload = number.to_bytes((number.bit_length() + 7) // 8, "big") if number else b""
    return b"\0" * (len(value) - len(value.lstrip("1"))) + payload


def jlp_plan(wallet_address: str) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"
    usdc = {"network": "solana:mainnet-beta", "reference": USDC_MINT, "decimals": 6}
    jlp = {"network": "solana:mainnet-beta", "reference": JLP_MINT, "decimals": 6}
    mint_input = {
        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
        "deposit_token": "USDC", "amount_atoms": "1000000", "minimum_jlp_atoms": "990000",
        "maximum_network_fee_lamports": "10000", "maximum_account_funding_lamports": "0",
    }
    burn_input = {
        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
        "receive_token": "USDC", "jlp_atoms": "1000000", "minimum_receive_atoms": "990000",
        "maximum_network_fee_lamports": "10000", "maximum_account_funding_lamports": "0",
    }
    common = {
        "maximum_input_atoms": "1000000",
        "minimum_output_per_input_numerator": "99",
        "minimum_output_per_input_denominator": "100",
        "maximum_network_fee_lamports": 10000,
        "maximum_account_funding_lamports": 0,
        "maximum_quote_lifetime_seconds": 600,
    }
    return json.dumps({
        "version": 1,
        "name": "Synthetic JLP mint and redeem",
        "steps": [
            {
                "step_id": "mint", "operation": "jupiter.jlp.mint", "input": mint_input,
                "permitted_variations": common,
                "completion": {"operation_succeeded": True}, "funding_dependencies": [],
            },
            {
                "step_id": "redeem", "operation": "jupiter.jlp.burn", "input": burn_input,
                "permitted_variations": common,
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [{
                    "producing_step_id": "mint", "resource": resource,
                    "asset": jlp, "funding_scope": "principal_only",
                }],
            },
        ],
        "aggregate_bounds": {
            "maximum_debits": [
                {"resource": resource, "amount": {"asset": usdc, "atoms": "1000000"}},
                {"resource": resource, "amount": {"asset": jlp, "atoms": "1000000"}},
            ],
            "maximum_costs": [{
                "location": {"kind": "cash", "resource": resource},
                "amount": {"asset": {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}, "atoms": "20000"},
                "treatment": "additional_debit",
            }],
        },
    }, separators=(",", ":"))


def perps_collateral_plan(wallet_address: str, position: str) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"
    usdc = {"network": "solana:mainnet-beta", "reference": USDC_MINT, "decimals": 6}
    native = {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}
    margin_usd = {"network": "solana:mainnet-beta", "reference": "jupiter:margin-usd", "decimals": 6}
    add = {
        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
        "position": position, "market": "SOL", "side": "long", "funding_token": "USDC",
        "collateral_amount_atoms": "1000000", "maximum_protocol_fee_atoms": "5000000",
        "maximum_protocol_fee_usd_atoms": "500000", "maximum_slippage_bps": 50,
        "maximum_network_fee_lamports": "10000", "maximum_account_funding_lamports": "3000000",
    }
    remove = {
        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
        "position": position, "receive_token": "USDC", "collateral_usd_atoms": "1000000",
        "minimum_receive_atoms": "994005", "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_slippage_bps": 50, "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "3000000",
    }
    common = {
        "maximum_size_usd_atoms": "0", "maximum_collateral_atoms": "1000000",
        "maximum_protocol_fee_usd_atoms": "500000", "maximum_protocol_fee_atoms": "5000000",
        "maximum_slippage_bps": 50, "maximum_network_fee_lamports": 10000,
        "maximum_account_funding_lamports": 3000000,
    }
    return json.dumps({
        "version": 1,
        "name": "Synthetic Perps collateral add and remove",
        "steps": [
            {"step_id": "add", "operation": "jupiter.perps.collateral.add", "input": add,
             "permitted_variations": common, "completion": {"operation_succeeded": True},
             "funding_dependencies": []},
            {"step_id": "remove", "operation": "jupiter.perps.collateral.remove", "input": remove,
             "permitted_variations": common, "completion": {"operation_succeeded": True},
             "funding_dependencies": []},
        ],
        "aggregate_bounds": {
            "maximum_debits": [
                {"resource": resource, "amount": {"asset": usdc, "atoms": "1000000"}},
                {"resource": resource, "amount": {"asset": native, "atoms": "6000000"}},
            ],
            "maximum_costs": [
                {
                    "location": {"kind": "cash", "resource": resource},
                    "amount": {"asset": native, "atoms": "20000"},
                    "treatment": "additional_debit",
                },
                {
                    "location": {
                        "kind": "position", "resource": resource,
                        "position": f"{position}:collateral", "position_kind": "locked_collateral",
                    },
                    "amount": {"asset": margin_usd, "atoms": "500000"},
                    "treatment": "included_in_debit",
                },
                {
                    "location": {
                        "kind": "position", "resource": resource,
                        "position": f"{position}:collateral", "position_kind": "locked_collateral",
                    },
                    "amount": {"asset": margin_usd, "atoms": "500000"},
                    "treatment": "additional_debit",
                },
            ],
        },
    }, separators=(",", ":"))


def trigger_recovery_plan(wallet_address: str) -> str:
    resource = f"solana:mainnet-beta:{wallet_address}"
    usdc = {"network": "solana:mainnet-beta", "reference": USDC_MINT, "decimals": 6}
    native = {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}
    expires_at = int(time.time()) + 1800
    create_input = {
        "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
        "input_mint": USDC_MINT, "output_mint": INPUT_MINT,
        "making_amount_atoms": "100000", "taking_amount_atoms": "99000",
        "expired_at_unix": expires_at,
        "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "5000000",
    }
    cancel_input = {
        "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
        "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "0",
    }
    return json.dumps({
        "version": 1,
        "name": "Synthetic Trigger escrow and recovery",
        "steps": [
            {
                "step_id": "create", "operation": "jupiter.limit.create", "input": create_input,
                "permitted_variations": {
                    "maximum_making_atoms": "100000",
                    "minimum_taking_per_making_numerator": "99",
                    "minimum_taking_per_making_denominator": "100",
                    "maximum_network_fee_lamports": 10000,
                    "maximum_account_funding_lamports": 5000000,
                    "maximum_order_lifetime_seconds": 3600,
                },
                "completion": {"operation_succeeded": True}, "funding_dependencies": [],
            },
            {
                "step_id": "cancel", "operation": "jupiter.limit.cancel", "input": cancel_input,
                "permitted_variations": {
                    "maximum_network_fee_lamports": 10000,
                    "maximum_account_funding_lamports": 0,
                    "recovery_parent_step_id": "create",
                },
                "completion": {"operation_succeeded": True}, "funding_dependencies": [],
            },
        ],
        "aggregate_bounds": {
            "maximum_debits": [
                {"resource": resource, "amount": {"asset": usdc, "atoms": "100000"}},
                {"resource": resource, "amount": {"asset": native, "atoms": "5000000"}},
            ],
            "maximum_costs": [{
                "location": {"kind": "cash", "resource": resource},
                "amount": {"asset": native, "atoms": "20000"},
                "treatment": "additional_debit",
            }],
        },
    }, separators=(",", ":"))


def summary(record: dict) -> dict:
    effects = record.get("effects") or {}
    submission = record.get("submission") or {}
    preview = record.get("preview") or {}
    return {
        "operation": record.get("operation"),
        "id": record.get("id"),
        "status": record.get("status"),
        "route": (preview.get("binding") or {}).get("route"),
        "reference": submission.get("reference"),
        "proof_digest": effects.get("proof_digest"),
        "cluster": (record.get("input") or {}).get("cluster"),
        "maximum_slippage_bps": (record.get("input") or {}).get("maximum_slippage_bps"),
    }


def wallet_token_amount(snapshot: dict, mint: str) -> int:
    accounts = snapshot.get("token_accounts") or []
    return sum(
        int(account["amount_atoms"])
        for account in accounts
        if (account.get("asset") or {}).get("reference") == mint
    )


def account_funding_deltas(record: dict) -> list[str]:
    effects = (record.get("effects") or {}).get("effects") or []
    return [
        effect.get("delta", "")
        for effect in effects
        if effect.get("kind") == "position_change"
        and str(effect.get("position", "")).startswith("account-funding:")
    ]


def cash_debit_atoms(record: dict) -> list[str]:
    effects = (record.get("effects") or {}).get("effects") or []
    return [
        str((effect.get("amount") or {}).get("atoms"))
        for effect in effects
        if effect.get("kind") == "cash_debit"
    ]


def conversion_atoms(record: dict) -> tuple[str, str]:
    effects = (record.get("effects") or {}).get("effects") or []
    conversion = next(
        (effect for effect in effects if effect.get("kind") == "conversion"), None
    )
    if conversion is None:
        return "", ""
    return (
        str((conversion.get("spent") or {}).get("atoms", "")),
        str((conversion.get("received") or {}).get("atoms", "")),
    )


def pending_request_pnl_deltas(record: dict) -> list[str]:
    effects = (record.get("effects") or {}).get("effects") or []
    return [
        str(progress.get("delta"))
        for effect in effects
        if effect.get("kind") == "pending_request_progress"
        for progress in effect.get("realized_pnl", [])
    ]


def pending_request_pnl_rows(record: dict) -> list[dict]:
    effects = (record.get("effects") or {}).get("effects") or []
    return [
        progress
        for effect in effects
        if effect.get("kind") == "pending_request_progress"
        for progress in effect.get("realized_pnl", [])
    ]


def pending_request_credit_atoms(record: dict, mint: str) -> list[str]:
    effects = (record.get("effects") or {}).get("effects") or []
    amounts: list[str] = []
    for effect in effects:
        if effect.get("kind") != "pending_request_progress":
            continue
        for credit in effect.get("credits", []):
            if (credit.get("asset") or {}).get("reference") != mint:
                continue
            amount = credit.get("amount")
            if not isinstance(amount, str) or not amount.isdecimal():
                raise AssertionError(
                    "Pending request credits must serialize as decimal Quantity strings."
                )
            amounts.append(amount)
    return amounts


def has_verified_open_limit_progress(record: dict) -> bool:
    if record.get("status") != "pending" or record.get("error") is not None:
        return False
    verified = record.get("effects") or {}
    if not verified.get("proof_digest"):
        return False
    effects = verified.get("effects") or []
    escrow_open = any(
        effect.get("kind") == "escrow_position_progress"
        and effect.get("state") == "open"
        and int(effect.get("remaining_input", "0")) > 0
        for effect in effects
    )
    funding_open = any(
        effect.get("kind") == "account_funding_progress"
        and effect.get("state") == "open"
        and int(effect.get("remaining", "0")) > 0
        for effect in effects
    )
    return escrow_open and funding_open


def has_network_fee(record: dict, lamports: str = "5000") -> bool:
    effects = (record.get("effects") or {}).get("effects") or []
    return any(
        effect.get("kind") == "cost"
        and effect.get("category") == "solana_network_fee"
        and str((effect.get("amount") or {}).get("atoms")) == lamports
        for effect in effects
    )


def network_fee_lamports(record: dict) -> int:
    effects = (record.get("effects") or {}).get("effects") or []
    return sum(
        int((effect.get("amount") or {}).get("atoms", "0"))
        for effect in effects
        if effect.get("kind") == "cost"
        and effect.get("category") == "solana_network_fee"
    )


def has_perps_protocol_fee(record: dict, atoms: str = "1000") -> bool:
    effects = (record.get("effects") or {}).get("effects") or []
    return any(
        effect.get("kind") == "cost"
        and effect.get("category") == "jupiter_perps_protocol_fee"
        and str((effect.get("amount") or {}).get("atoms")) == atoms
        for effect in effects
    )


def restart_fixture(environment_name: str, env: dict[str, str]) -> None:
    stop = subprocess.run(
        [sys.executable, str(DEV), "stop", environment_name],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if stop.returncode:
        raise RuntimeError(stop.stderr or stop.stdout or "Synthetic environment stop failed.")
    restart = subprocess.run(
        [sys.executable, str(DEV), "start", environment_name,
         "--mode", "synthetic", "--fixture", "solana"],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        timeout=180,
    )
    if restart.returncode:
        raise RuntimeError(restart.stderr or restart.stdout or "Synthetic fixture restart failed.")


def journal_reservation_count(state_home: Path, environment_name: str) -> int:
    """Read the stopped/running synthetic journal without changing it."""
    for metadata_path in state_home.rglob("environment.json"):
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if metadata.get("name") != environment_name:
                continue
            database = Path(metadata["database"]).resolve(strict=True)
            uri = f"{database.as_uri()}?mode=ro"
            with sqlite3.connect(uri, uri=True, timeout=10) as connection:
                return int(connection.execute("SELECT COUNT(*) FROM reservations").fetchone()[0])
        except (OSError, KeyError, sqlite3.Error, ValueError, TypeError):
            continue
    raise AssertionError("Could not inspect the synthetic journal for paired TPSL reservations.")


def run_paired_tpsl_cli(
    environment_name: str,
    env: dict[str, str],
    initial_usdc_atoms: int,
    initial_lamports: int,
) -> dict[str, object]:
    """Exercise paired partial TP/full SL, restart and exact restoration publicly."""
    positions_input = {"network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal"}
    starting = call(
        environment_name,
        ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
        env,
    )
    if starting.get("positions"):
        raise AssertionError("The paired TPSL rehearsal must start without an open position.")

    cases: dict[str, object] = {}
    expected_usdc = initial_usdc_atoms
    expected_lamports = initial_lamports
    tp_payout = 6_104_000
    sl_payout = 4_894_000
    half_size = 6_050_000
    half_collateral = 5_499_500
    for side, creation_order, tp_trigger, sl_trigger in (
        ("long", "tp-first", "110000000", "90000000"),
        ("short", "sl-first", "90000000", "110000000"),
    ):
        prefix = f"solana-tpsl-paired-{side}"
        opened = call(
            environment_name,
            ["execute", "jupiter.perps.increase_market", "--input", json.dumps({
                **positions_input, "market": "SOL", "side": side, "funding_token": "USDC",
                "collateral_amount_atoms": "11000000", "size_usd_atoms": "12100000",
                "maximum_protocol_fee_atoms": "5000000", "maximum_protocol_fee_usd_atoms": "500000",
                "maximum_slippage_bps": 50, "maximum_network_fee_lamports": "10000",
                "maximum_account_funding_lamports": "6000000",
            }), "--id", f"{prefix}-open", "--json"],
            env,
        )
        if opened.get("status") != "completed" or network_fee_lamports(opened) != 5_000:
            raise AssertionError(f"{side} paired position did not open with its exact fee: {json.dumps(opened, sort_keys=True)}")
        position_read = call(
            environment_name,
            ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
            env,
        )
        matching = [row for row in position_read.get("positions", []) if row.get("side") == side]
        if len(matching) != 1:
            raise AssertionError(f"{side} paired position is not uniquely readable: {json.dumps(position_read, sort_keys=True)}")
        position = matching[0]["position"]

        def create(request_type: str, parent_id: str | None, label: str) -> dict:
            payload = {
                **positions_input, "position": position, "request_type": request_type,
                "trigger_price_usd_atoms": tp_trigger if request_type == "tp" else sl_trigger,
                "receive_token": "USDC", "entire_position": request_type == "sl",
                "maximum_protocol_fee_usd_atoms": "500000",
                "maximum_network_fee_lamports": "15000",
                "maximum_account_funding_lamports": "6000000",
            }
            if request_type == "tp":
                payload["size_usd_atoms"] = str(half_size)
            if parent_id is not None:
                payload["protection_parent_operation_id"] = parent_id
            result = call(
                environment_name,
                ["execute", "jupiter.perps.tpsl.create", "--input", json.dumps(payload),
                 "--id", f"{prefix}-{label}", "--json"],
                env,
            )
            if result.get("status") != "pending" or not (result.get("effects") or {}).get("proof_digest"):
                raise AssertionError(f"{side} {request_type} did not retain verified open funding: {json.dumps(result, sort_keys=True)}")
            if network_fee_lamports(result) != 15_000:
                raise AssertionError(f"{side} {request_type} placement fee differs from exact wallet-paid receipt.")
            if not isinstance(result.get("id"), str) or not isinstance((result.get("preview") or {}).get("binding", {}).get("request"), str):
                raise AssertionError(f"{side} {request_type} omitted durable request identity.")
            return result

        if creation_order == "tp-first":
            tp = create("tp", None, "tp-create")
            sl = create("sl", tp["id"], "sl-create")
        else:
            sl = create("sl", None, "sl-create")
            tp = create("tp", sl["id"], "tp-create")
        tp_request = tp["preview"]["binding"]["request"]
        sl_request = sl["preview"]["binding"]["request"]

        fill_path = Path(env["FIN_DEV_SOLANA_PERPS_TPSL_FILL_FILE"])
        fill_path.write_text(json.dumps({
            "request": tp_request,
            "fill": {
                "size_delta": half_size, "post_size": half_size,
                "post_collateral": half_collateral,
                "execution_price": 110_000_000 if side == "long" else 90_000_000,
                "pnl_usd": 605_000, "has_profit": True,
                "payout_usd": tp_payout, "payout_atoms": tp_payout, "fee_usd": 500,
            },
        }), encoding="utf-8")
        tp_settled = call_pending(environment_name, ["status", tp["id"], "--json"], env)
        if tp_settled.get("status") != "completed" or tp_settled.get("error") is not None:
            raise AssertionError(f"{side} partial TP did not settle from finalized keeper proof: {json.dumps(tp_settled, sort_keys=True)}")
        if pending_request_pnl_deltas(tp_settled) != ["605000"]:
            raise AssertionError(f"{side} partial TP realized PnL is not exact.")
        if pending_request_credit_atoms(tp_settled, USDC_MINT) != [str(tp_payout)]:
            raise AssertionError(f"{side} partial TP payout is not exact.")
        if network_fee_lamports(tp_settled) != 15_000:
            raise AssertionError("The partial TP record must retain only its placement fee.")

        records_before_restart = len(json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", []))
        restart_fixture(environment_name, env)
        sibling = sl if tp["id"] != sl.get("id") and creation_order == "tp-first" else sl
        sibling_open = call_pending(environment_name, ["status", sibling["id"], "--json"], env)
        if sibling_open.get("status") != "pending" or sibling_open.get("error") is not None or not (sibling_open.get("effects") or {}).get("proof_digest"):
            raise AssertionError(f"{side} sibling did not remain verified/open after restart: {json.dumps(sibling_open, sort_keys=True)}")
        if len(json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", [])) != records_before_restart:
            raise AssertionError("Restarted sibling status reconciliation broadcast a duplicate transaction.")

        fill_path.write_text(json.dumps({
            "request": sl_request,
            "fill": {
                "size_delta": half_size, "post_size": 0, "post_collateral": 0,
                "execution_price": 90_000_000 if side == "long" else 110_000_000,
                "pnl_usd": 605_000, "has_profit": False,
                "payout_usd": sl_payout, "payout_atoms": sl_payout, "fee_usd": 500,
            },
        }), encoding="utf-8")
        sl_settled = call_pending(environment_name, ["status", sl["id"], "--json"], env)
        if sl_settled.get("status") != "completed" or sl_settled.get("error") is not None:
            raise AssertionError(f"{side} full SL did not settle from its exact keeper proof: {json.dumps(sl_settled, sort_keys=True)}")
        if pending_request_pnl_deltas(sl_settled) != ["-605000"]:
            raise AssertionError(f"{side} full SL realized PnL is not exact and signed.")
        if pending_request_credit_atoms(sl_settled, USDC_MINT) != [str(sl_payout)]:
            raise AssertionError(f"{side} full SL payout is not exact.")
        if network_fee_lamports(sl_settled) != 15_000:
            raise AssertionError("The full SL record must retain only its placement fee.")
        final_positions = call(
            environment_name,
            ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
            env,
        )
        if any(row.get("position") == position for row in final_positions.get("positions", [])):
            raise AssertionError(f"{side} paired keeper proof left the exact position open.")
        expected_usdc -= 2_000
        expected_lamports -= 35_000
        current_balances = call(
            environment_name,
            ["read", "--input", json.dumps(positions_input), "wallet.balances", "--json"],
            env,
        )
        if wallet_token_amount(current_balances, USDC_MINT) != expected_usdc:
            raise AssertionError(f"{side} paired wallet USDC does not reconcile to its exact fees and payouts.")
        if int(current_balances["native"]["amount_atoms"]) != expected_lamports:
            raise AssertionError(f"{side} paired native balance does not reconcile to exact wallet-paid fees and rent refunds.")
        cases[side] = {
            "creation_order": creation_order,
            "position": position,
            "tp_request": tp_request,
            "sl_request": sl_request,
            "open": summary(opened),
            "tp_create": summary(tp),
            "sl_create": summary(sl),
            "tp_fill": summary(tp_settled),
            "sl_fill": summary(sl_settled),
            "restoration_usdc_atoms": str(expected_usdc),
            "restoration_lamports": str(expected_lamports),
        }
    if expected_usdc != initial_usdc_atoms - 4_000:
        raise AssertionError("Paired TPSL final USDC differs from exact two-open/two-keeper fees.")
    if expected_lamports != initial_lamports - 70_000:
        raise AssertionError("Paired TPSL final SOL differs from the six exact wallet-paid signatures.")
    if journal_reservation_count(Path(env["FIN_DEV_STATE_HOME"]), environment_name) != 0:
        raise AssertionError("Paired TPSL terminal operations left journal reservations behind.")
    return cases


def backup_stopped_journal(
    state_home: Path, environment_name: str, failure_dir: Path
) -> None:
    """Use fin-dev's saved environment metadata to find and back up its journal."""
    state_root = state_home.resolve()
    expected_worktree = str(REPO.resolve())
    selected_database: Path | None = None
    for metadata_path in state_root.rglob("environment.json"):
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if (
                metadata.get("name") != environment_name
                or metadata.get("worktree") != expected_worktree
            ):
                continue
            database = Path(metadata["database"]).resolve(strict=True)
            database.relative_to(state_root)
            if not database.is_file():
                continue
            selected_database = database
            failure_dir.mkdir(parents=True, exist_ok=True)
            destination = failure_dir / "operations.sqlite"
            temporary = failure_dir / "operations.sqlite.tmp"
            temporary.unlink(missing_ok=True)
            # Use SQLite's online backup API so WAL contents are included. Open
            # the source read-only and validate the destination before making
            # it visible as the preserved journal.
            source_uri = f"{database.as_uri()}?mode=ro"
            with sqlite3.connect(source_uri, uri=True, timeout=10) as source:
                with sqlite3.connect(str(temporary), timeout=10) as backup:
                    source.backup(backup)
                    integrity = backup.execute("PRAGMA quick_check").fetchone()
                    if integrity != ("ok",):
                        raise sqlite3.DatabaseError(
                            f"journal backup integrity check failed: {integrity!r}"
                        )
                    if backup.execute(
                        "SELECT count(*) FROM sqlite_master WHERE type='table'"
                    ).fetchone()[0] == 0:
                        raise sqlite3.DatabaseError("journal backup contains no tables")
            temporary.replace(destination)
            (failure_dir / "database-copy-mode.txt").write_text(
                "sqlite-online-backup\n", encoding="utf-8"
            )
            return
        except (OSError, KeyError, ValueError, TypeError, sqlite3.Error) as error:
            (failure_dir / "operations.sqlite.tmp").unlink(missing_ok=True)
            (failure_dir / "operations.sqlite").unlink(missing_ok=True)
            (failure_dir / "database-backup-error.txt").write_text(
                str(error), encoding="utf-8"
            )
            if selected_database is not None:
                copy_stopped_journal_files(selected_database, failure_dir)
            return


def copy_stopped_journal_files(database: Path, failure_dir: Path) -> list[str]:
    """Preserve the stopped SQLite DB and its WAL sidecars as one evidence set."""
    copied = []
    for suffix in ("", "-wal", "-shm"):
        source = Path(f"{database}{suffix}")
        if source.is_file():
            name = f"operations.sqlite{suffix}"
            shutil.copyfile(source, failure_dir / name)
            copied.append(name)
    if "operations.sqlite" in copied:
        (failure_dir / "database-copy-mode.txt").write_text(
            "stopped-raw-copy; preserve SQLite sidecars together\n"
            + "\n".join(copied)
            + "\n",
            encoding="utf-8",
        )
    return copied


def test_stopped_journal_backup() -> None:
    """Exercise the failure-evidence backup path without a running broker."""
    with tempfile.TemporaryDirectory(prefix="fin-solana-backup-test-") as temp:
        root = Path(temp)
        state_home = root / "state"
        environment_dir = state_home / "worktree-key" / "synthetic"
        environment_dir.mkdir(parents=True)
        database = environment_dir / "operations.sqlite"
        with sqlite3.connect(str(database)) as connection:
            connection.execute("CREATE TABLE durable_test (value TEXT NOT NULL)")
            connection.execute("INSERT INTO durable_test VALUES ('pending')")
        metadata = {
            "name": "synthetic",
            "worktree": str(REPO.resolve()),
            "database": str(database),
        }
        (environment_dir / "environment.json").write_text(json.dumps(metadata))
        evidence = root / "evidence"
        evidence.mkdir()
        backup_stopped_journal(state_home, "synthetic", evidence)
        with sqlite3.connect(str(evidence / "operations.sqlite")) as connection:
            assert connection.execute("PRAGMA quick_check").fetchone() == ("ok",)
            assert connection.execute(
                "SELECT value FROM durable_test"
            ).fetchone() == ("pending",)
        raw = root / "raw.sqlite"
        raw.write_bytes(b"synthetic database bytes")
        Path(f"{raw}-wal").write_bytes(b"synthetic wal bytes")
        Path(f"{raw}-shm").write_bytes(b"synthetic shm bytes")
        raw_evidence = root / "raw-evidence"
        raw_evidence.mkdir()
        assert copy_stopped_journal_files(raw, raw_evidence) == [
            "operations.sqlite",
            "operations.sqlite-wal",
            "operations.sqlite-shm",
        ]
        assert (raw_evidence / "operations.sqlite-wal").read_bytes() == b"synthetic wal bytes"
        assert "stopped-raw-copy" in (raw_evidence / "database-copy-mode.txt").read_text()


def test_perps_collateral_plan_shape() -> None:
    plan = json.loads(perps_collateral_plan("synthetic-wallet", "synthetic-position"))
    assert [step["operation"] for step in plan["steps"]] == [
        "jupiter.perps.collateral.add", "jupiter.perps.collateral.remove"
    ]
    for step in plan["steps"]:
        assert step["input"]["cluster"] == "mainnet-beta"
        assert step["input"]["position"] == "synthetic-position"
        assert step["permitted_variations"]["maximum_network_fee_lamports"] == 10_000
    assert plan["steps"][0]["input"]["collateral_amount_atoms"] == "1000000"
    assert plan["steps"][1]["input"]["collateral_usd_atoms"] == "1000000"
    assert plan["steps"][1]["input"]["minimum_receive_atoms"] == "994005"
    margin_cost_caps = {
        bound["treatment"]: (
            bound["location"], bound["amount"]["asset"], bound["amount"]["atoms"]
        )
        for bound in plan["aggregate_bounds"]["maximum_costs"]
        if bound["location"].get("position_kind") == "locked_collateral"
    }
    assert set(margin_cost_caps) == {"included_in_debit", "additional_debit"}
    assert all(
        location == {
            "kind": "position", "resource": "solana:mainnet-beta:synthetic-wallet",
            "position": "synthetic-position:collateral", "position_kind": "locked_collateral",
        }
        and asset == {
            "network": "solana:mainnet-beta", "reference": "jupiter:margin-usd", "decimals": 6,
        }
        and atoms == "500000"
        for location, asset, atoms in margin_cost_caps.values()
    )
    assert sum(int(atoms) for _, _, atoms in margin_cost_caps.values()) == 1_000_000


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run isolated public-CLI Solana/Jupiter synthetic rehearsals."
    )
    selectors = parser.add_mutually_exclusive_group()
    selectors.add_argument("--perps-only", action="store_true", help="Run Perps market round trips.")
    selectors.add_argument("--perps-collateral-only", action="store_true", help="Run Perps collateral add/remove lifecycle.")
    selectors.add_argument("--perps-limit-only", action="store_true", help="Run Perps limit lifecycle.")
    selectors.add_argument("--perps-tpsl-only", action="store_true", help="Run Perps TP/SL keeper settlement cases.")
    selectors.add_argument("--perps-tpsl-paired-only", action="store_true", help="Run paired partial TP/full SL cases across restart.")
    selectors.add_argument("--perps-tpsl-plan-only", action="store_true", help="Run the future-funded TP/SL open/create/update/cancel/close plan.")
    selectors.add_argument("--perps-limit-plan-only", action="store_true", help="Run the limit create/update/restart/cancel/refund named plan.")
    selectors.add_argument("--jlp-only", action="store_true", help="Run JLP mint/redeem cases.")
    selectors.add_argument("--trigger-only", action="store_true", help="Run Trigger recovery plan.")
    selectors.add_argument("--trigger-fill-only", action="store_true", help="Run Trigger partial and full fill settlement.")
    selectors.add_argument(
        "--expiry-only",
        nargs="?",
        const="dispatch-once",
        choices=("revalidate-once", "dispatch-once", "revalidate-every", "dispatch-every", "uncertain"),
        help="Run a synthetic named-plan expiry refresh case.",
    )
    parser.add_argument(
        "--expiry-route", choices=("router", "meta_aggregator"), default="router",
        help="Select the synthetic Jupiter path for --expiry-only (default: router).",
    )
    args = parser.parse_args()
    test_stopped_journal_backup()
    test_perps_collateral_plan_shape()
    perps_only = args.perps_only
    perps_collateral_only = args.perps_collateral_only
    perps_limit_only = args.perps_limit_only
    perps_tpsl_only = args.perps_tpsl_only
    perps_tpsl_paired_only = args.perps_tpsl_paired_only
    perps_tpsl_plan_only = args.perps_tpsl_plan_only
    perps_limit_plan_only = args.perps_limit_plan_only
    jlp_only = args.jlp_only
    trigger_only = args.trigger_only
    trigger_fill_only = args.trigger_fill_only
    expiry_only = args.expiry_only
    records: dict[str, dict] = {}
    with tempfile.TemporaryDirectory(prefix="fin-solana-rehearsal-") as temp:
        state_home = Path(temp) / "state"
        environment_name = "solana-rehearsal"
        env = dict(
            os.environ,
            FIN_DEV_STATE_HOME=str(state_home),
            FIN_DEV_SOLANA_FIXTURE_STATE=str(state_home / "solana-fixture-ledger.json"),
            FIN_DEV_SOLANA_FIXTURE_JUPITER_LAYOUT="auto_spot_v2",
            FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE=str(state_home / "remove-wsol-ata"),
            FIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE=str(state_home / "next-meta-router"),
            FIN_DEV_SOLANA_PERPS_LIMIT_FILL_FILE=str(state_home / "perps-limit-fill-request"),
            FIN_DEV_SOLANA_PERPS_TPSL_FILL_FILE=str(state_home / "perps-tpsl-fill-request"),
            FIN_DEV_SOLANA_TRIGGER_FILL_FILE=str(state_home / "trigger-fill-request"),
            FIN_DEV_SOLANA_TRIGGER_EXTERNAL_ORDER_FILE=str(state_home / "trigger-external-order-request"),
            FIN_DEV_SOLANA_LOSE_RESPONSE_TRANSFER_LAMPORTS="100000",
            FIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_ATOMS="1000000",
            FIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_MINT=OUTPUT_MINT,
            FIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_OUTPUT_MINT=PYUSD_MINT,
            FIN_DEV_SOLANA_LOSE_RESPONSE_META_REQUEST_ID=(
                f"synthetic-order-1000000-{PYUSD_MINT}-{OUTPUT_MINT}"
            ),
        )
        if expiry_only:
            env["FIN_DEV_SOLANA_FIXTURE_EXPIRY_MODE"] = expiry_only
            if expiry_only == "uncertain":
                if args.expiry_route == "meta_aggregator":
                    env["FIN_DEV_SOLANA_LOSE_RESPONSE_META_REQUEST_ID"] = (
                        f"synthetic-order-1000000-{OUTPUT_MINT}-{PYUSD_MINT}"
                    )
            elif expiry_only:
                env["FIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_ATOMS"] = "999999999"
                env["FIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_INPUT_MINT"] = OUTPUT_MINT
                env["FIN_DEV_SOLANA_LOSE_RESPONSE_ROUTER_OUTPUT_MINT"] = PYUSD_MINT
        if perps_only or perps_collateral_only or perps_limit_only or perps_tpsl_only or perps_tpsl_paired_only or perps_tpsl_plan_only or perps_limit_plan_only:
            env["FIN_DEV_SOLANA_PERPS_REHEARSAL"] = "1"
        started = False
        evidence: dict[str, object] = {"evidence_class": "public_cli_synthetic", "live_network": False}
        try:
            start = subprocess.run(
                [sys.executable, str(DEV), "start", environment_name,
                 "--mode", "synthetic", "--fixture", "solana"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
                timeout=180,
            )
            if start.returncode:
                raise RuntimeError(start.stderr or start.stdout or "Synthetic fixture startup failed.")
            started = True
            balance = call(
                environment_name,
                ["read", "--input", json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}), "wallet.balances", "--json"],
                env,
            )
            if balance.get("cluster") != "mainnet-beta" or balance.get("observed_commitment") != "finalized":
                raise AssertionError("Public wallet snapshot omitted its verified Solana scope.")
            initial_usdc_atoms = wallet_token_amount(balance, OUTPUT_MINT)
            initial_pyusd_atoms = wallet_token_amount(balance, PYUSD_MINT)
            initial_native_lamports = int(balance["native"]["amount_atoms"])

            if expiry_only:
                evidence["expiry"] = run_expiry_rehearsal(
                    environment_name, env, expiry_only, balance["wallet"], args.expiry_route
                )
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_tpsl_paired_only:
                evidence["perps_tpsl_paired"] = run_paired_tpsl_cli(
                    environment_name, env, initial_usdc_atoms, initial_native_lamports
                )
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_tpsl_plan_only:
                evidence["perps_tpsl_plan"] = run_named_plan_tpsl(
                    environment_name, env, initial_usdc_atoms, initial_native_lamports
                )
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_limit_plan_only:
                evidence["perps_limit_plan"] = run_named_plan_limit(
                    environment_name, env, initial_usdc_atoms, initial_native_lamports
                )
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if trigger_only:
                plan_input = trigger_recovery_plan(balance["wallet"])
                parsed_plan = json.loads(plan_input)
                plan_preview = call(environment_name, ["plans", "preview", "--input", plan_input, "--valid-for", "10m", "--json"], env)
                if not plan_preview.get("review_hash"):
                    raise AssertionError("Trigger recovery plan preview omitted its review hash.")
                plan_approval = call(
                    environment_name,
                    ["plans", "authorize", "--input", plan_input, "--id", "solana-trigger-recovery-plan",
                     "--valid-for", "10m", "--review-hash", plan_preview["review_hash"], "--json"],
                    env,
                )
                plan_id = plan_approval.get("plan_id") or plan_approval.get("id")
                if not plan_id:
                    raise AssertionError("Trigger recovery plan authorization omitted its durable plan ID.")

                create_id = "solana-trigger-create-attempt"
                create_input = parsed_plan["steps"][0]["input"]
                create = call(
                    environment_name,
                    ["execute", "jupiter.limit.create", "--input", json.dumps(create_input),
                     "--id", create_id, "--plan", plan_id, "--step", "create", "--json"],
                    env,
                )
                records["trigger_create"] = create
                if create.get("status") != "pending" or not create.get("effects", {}).get("proof_digest"):
                    raise AssertionError(
                        "Trigger placement must prove escrow funding while staying pending: "
                        + json.dumps(create, sort_keys=True)
                    )
                create_operation_id = create.get("id")
                if not isinstance(create_operation_id, str) or not create_operation_id:
                    raise AssertionError(
                        "Trigger placement omitted its durable operation UUID: "
                        + json.dumps(create, sort_keys=True)
                    )
                order_key = create.get("order_key") or (
                    (create.get("preview") or {}).get("binding") or {}
                ).get("order_key")
                if not order_key:
                    raise AssertionError("Trigger placement omitted its exact verified order PDA.")

                order_status_input = {
                    "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
                    "order_key": order_key,
                }
                open_order = call_pending(
                    environment_name,
                    ["read", "--input", json.dumps(order_status_input), "jupiter.limit.status", "--json"],
                    env,
                )
                records["trigger_open_order_read"] = open_order
                if (
                    open_order.get("state") != "open_escrowed"
                    or open_order.get("remaining_making_atoms") != "100000"
                ):
                    raise AssertionError(
                        "Finalized Trigger order read did not confirm the exact open escrow: "
                        + json.dumps(open_order, sort_keys=True)
                    )

                plan_status = call(environment_name, ["plans", "status", plan_id, "--json"], env)
                if plan_status.get("next_eligible_step_id") != "cancel":
                    raise AssertionError(
                        "The authorized recovery step did not become eligible for the unresolved parent: "
                        + json.dumps(plan_status, sort_keys=True)
                    )
                cancel_input = parsed_plan["steps"][1]["input"]
                cancel = call(
                    environment_name,
                    ["execute", "jupiter.limit.cancel", "--input", json.dumps(cancel_input),
                     "--id", "solana-trigger-cancel-attempt", "--plan", plan_id, "--step", "cancel", "--json"],
                    env,
                )
                records["trigger_cancel"] = cancel
                if cancel.get("status") != "completed" or not cancel.get("effects", {}).get("proof_digest"):
                    raise AssertionError("Trigger cancellation did not settle from exact finalized proof.")

                create_recovered = call(
                    environment_name, ["status", create_operation_id, "--json"], env
                )
                records["trigger_create_recovered"] = create_recovered
                if create_recovered.get("status") != "completed" or not (create_recovered.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        "The original Trigger escrow operation did not settle after cancellation proof: "
                        + json.dumps(create_recovered, sort_keys=True)
                    )
                escrow = [
                    effect for effect in (create_recovered.get("effects") or {}).get("effects", [])
                    if effect.get("kind") == "escrow_progress"
                ]
                if len(escrow) != 1 or escrow[0].get("state") != "cancelled" or escrow[0].get("input_refunded") != "100000":
                    raise AssertionError("The original Trigger record did not show exact cancelled/refunded escrow state.")
                final_order = call_pending(
                    environment_name,
                    ["read", "--input", json.dumps(order_status_input), "jupiter.limit.status", "--json"],
                    env,
                )
                if (
                    final_order.get("order_key") != order_key
                    or final_order.get("state") != "cancelled"
                    or final_order.get("cancel_event_seen") is not True
                    or len(final_order.get("verified_cancel_events") or []) != 1
                    or final_order.get("total_filled_making_atoms") != "0"
                    or final_order.get("remaining_making_atoms") != "100000"
                ):
                    raise AssertionError(
                        "Finalized Trigger status did not confirm the exact cancelled order: "
                        + json.dumps(final_order, sort_keys=True)
                    )
                restored = call(
                    environment_name,
                    ["read", "--input", json.dumps({
                        "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
                    }), "wallet.balances", "--json"],
                    env,
                )
                if wallet_token_amount(restored, USDC_MINT) != wallet_token_amount(balance, USDC_MINT):
                    raise AssertionError("Trigger cancellation did not restore the exact input-token balance.")
                evidence["trigger"] = {
                    "plan_id": plan_id,
                    "order_key": order_key,
                    "open_order": open_order.get("state"),
                    "create": summary(create),
                    "cancel": summary(cancel),
                    "reconciled_original": summary(create_recovered),
                    "refunded_input_atoms": escrow[0]["input_refunded"],
                }
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if trigger_fill_only:
                evidence["trigger_fills"] = run_partial_then_full_fill(
                    environment_name,
                    env,
                    balance["wallet"],
                    balance,
                    USDC_MINT,
                    INPUT_MINT,
                    call,
                    call_pending,
                    summary,
                    wallet_token_amount,
                )
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_tpsl_only:
                positions_input = {
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "wallet": "solana-rehearsal",
                }
                starting = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if starting.get("positions"):
                    raise AssertionError("The isolated TPSL rehearsal must start without an open position.")
                cases: dict[str, object] = {}
                expected_tpsl_usdc = initial_usdc_atoms
                for side, request_type, trigger, expected_pnl, expected_signed_pnl, expected_payout in (
                    ("long", "sl", "90000000", "10998000", "-10998000", "0"),
                    ("short", "tp", "95000000", "1000000", "1000000", "11998000"),
                ):
                    prefix = f"solana-tpsl-{side}"
                    opened = call(
                        environment_name,
                        ["execute", "jupiter.perps.increase_market", "--input", json.dumps({
                            **positions_input,
                            "market": "SOL",
                            "side": side,
                            "funding_token": "USDC",
                            "collateral_amount_atoms": "11000000",
                            "size_usd_atoms": "12100000",
                            "maximum_protocol_fee_atoms": "5000000",
                            "maximum_protocol_fee_usd_atoms": "500000",
                            "maximum_slippage_bps": 50,
                            "maximum_network_fee_lamports": "10000",
                            "maximum_account_funding_lamports": "6000000",
                        }), "--id", f"{prefix}-open", "--json"],
                        env,
                    )
                    records[f"tpsl_{side}_open"] = opened
                    if opened.get("status") != "completed":
                        raise AssertionError(f"The {side} TPSL position did not open: {json.dumps(opened, sort_keys=True)}")
                    if network_fee_lamports(opened) != 5_000:
                        raise AssertionError(f"The {side} market open did not record its exact wallet signature fee.")
                    positions = call(
                        environment_name,
                        ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                        env,
                    )
                    matches = [item for item in positions.get("positions", []) if item.get("side") == side]
                    if len(matches) != 1:
                        raise AssertionError(f"The {side} TPSL position was not uniquely readable: {json.dumps(positions, sort_keys=True)}")
                    position = matches[0]["position"]
                    placed = call(
                        environment_name,
                        ["execute", "jupiter.perps.tpsl.create", "--input", json.dumps({
                            **positions_input,
                            "position": position,
                            "request_type": request_type,
                            "trigger_price_usd_atoms": trigger,
                            "receive_token": "USDC",
                            "entire_position": True,
                            "maximum_protocol_fee_usd_atoms": "500000",
                            "maximum_network_fee_lamports": "15000",
                            "maximum_account_funding_lamports": "6000000",
                        }), "--id", f"{prefix}-create", "--json"],
                        env,
                    )
                    records[f"tpsl_{side}_create"] = placed
                    if placed.get("status") != "pending" or not (placed.get("effects") or {}).get("proof_digest"):
                        raise AssertionError(f"The {side} TPSL placement did not remain pending with verified funding: {json.dumps(placed, sort_keys=True)}")
                    if network_fee_lamports(placed) != 15_000:
                        raise AssertionError("TPSL placement must charge only the verified 15,000-lamport wallet fee.")
                    operation_id = placed.get("id")
                    binding = (placed.get("preview") or {}).get("binding") or {}
                    request = binding.get("request")
                    if not isinstance(operation_id, str) or not isinstance(request, str):
                        raise AssertionError("TPSL create did not expose its durable operation and request identities.")
                    updated_trigger = "91000000" if side == "long" else "94000000"
                    updated = call(
                        environment_name,
                        ["execute", "jupiter.perps.tpsl.update", "--input", json.dumps({
                            **positions_input,
                            "request": request,
                            "original_operation_id": operation_id,
                            "trigger_price_usd_atoms": updated_trigger,
                            "maximum_network_fee_lamports": "15000",
                        }), "--id", f"{prefix}-update", "--json"],
                        env,
                    )
                    records[f"tpsl_{side}_update"] = updated
                    if updated.get("status") != "completed" or not (updated.get("effects") or {}).get("proof_digest"):
                        raise AssertionError(f"The {side} TPSL trigger update did not settle from its exact proof: {json.dumps(updated, sort_keys=True)}")
                    if network_fee_lamports(updated) != 15_000:
                        raise AssertionError("TPSL update must charge only its exact three-signature fee.")
                    tx_count_before_restart = len(
                        json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                        .get("transactions", [])
                    )
                    restart_fixture(environment_name, env)
                    pending = call_pending(
                        environment_name, ["status", operation_id, "--json"], env
                    )
                    if (
                        pending.get("status") != "pending"
                        or pending.get("error") is not None
                        or not (pending.get("effects") or {}).get("proof_digest")
                    ):
                        raise AssertionError(
                            "TPSL placement should remain verified and pending after restart: "
                            + json.dumps(pending, sort_keys=True)
                        )
                    records[f"tpsl_{side}_restart_pending"] = pending
                    tx_count_after_restart = len(
                        json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                        .get("transactions", [])
                    )
                    if tx_count_after_restart != tx_count_before_restart:
                        raise AssertionError(
                            "Status reconciliation after restart broadcast or duplicated a TPSL transaction."
                        )

                    cancelled = None
                    cancelled_parent = None
                    if side == "long":
                        if (
                            pending.get("status") != "pending"
                            or pending.get("error") is not None
                            or not (pending.get("effects") or {}).get("proof_digest")
                        ):
                            raise AssertionError("The long TPSL create did not retain verified open state after restart.")
                        records["tpsl_long_restarted_open"] = pending
                        cancelled = call(
                            environment_name,
                            ["execute", "jupiter.perps.tpsl.cancel", "--input", json.dumps({
                                **positions_input,
                                "request": request,
                                "original_operation_id": operation_id,
                                "maximum_network_fee_lamports": "15000",
                            }), "--id", f"{prefix}-cancel", "--json"],
                            env,
                        )
                        records["tpsl_long_cancel"] = cancelled
                        if cancelled.get("status") != "completed" or not (cancelled.get("effects") or {}).get("proof_digest"):
                            raise AssertionError(f"The no-fill TPSL cancel did not settle from exact close proof: {json.dumps(cancelled, sort_keys=True)}")
                        if network_fee_lamports(cancelled) != 5_000:
                            raise AssertionError("The wallet-paid TPSL cancel should charge exactly one signature fee.")
                        cancelled_parent = call_pending(environment_name, ["status", operation_id, "--json"], env)
                        records["tpsl_long_cancelled_parent"] = cancelled_parent
                        if (
                            cancelled_parent.get("status") != "completed"
                            or cancelled_parent.get("error") is not None
                            or not (cancelled_parent.get("effects") or {}).get("proof_digest")
                        ):
                            raise AssertionError(f"The original no-fill TPSL did not reconcile its cancel/refund child: {json.dumps(cancelled_parent, sort_keys=True)}")
                        placed = call(
                            environment_name,
                            ["execute", "jupiter.perps.tpsl.create", "--input", json.dumps({
                                **positions_input,
                                "position": position,
                                "request_type": request_type,
                                "trigger_price_usd_atoms": trigger,
                                "receive_token": "USDC",
                                "entire_position": True,
                                "maximum_protocol_fee_usd_atoms": "500000",
                                "maximum_network_fee_lamports": "15000",
                                "maximum_account_funding_lamports": "6000000",
                            }), "--id", f"{prefix}-fill-create", "--json"],
                            env,
                        )
                        records["tpsl_long_fill_create"] = placed
                        if placed.get("status") != "pending" or not (placed.get("effects") or {}).get("proof_digest"):
                            raise AssertionError(f"A fresh long TPSL did not remain pending with verified funding: {json.dumps(placed, sort_keys=True)}")
                        operation_id = placed.get("id")
                        binding = (placed.get("preview") or {}).get("binding") or {}
                        request = binding.get("request")
                        if not isinstance(operation_id, str) or not isinstance(request, str):
                            raise AssertionError("The replacement long TPSL omitted its durable identities.")
                    if side == "long":
                        tx_count_before_new_pending = len(
                            json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                            .get("transactions", [])
                        )
                        pending = call_pending(
                            environment_name, ["status", operation_id, "--json"], env
                        )
                        if (
                            pending.get("status") != "pending"
                            or pending.get("error") is not None
                            or not (pending.get("effects") or {}).get("proof_digest")
                        ):
                            raise AssertionError(
                                "The replacement long TPSL did not retain verified open state: "
                                + json.dumps(pending, sort_keys=True)
                            )
                        if len(
                            json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                            .get("transactions", [])
                        ) != tx_count_before_new_pending:
                            raise AssertionError(
                                "Reading the replacement TPSL status broadcast another transaction."
                            )
                    Path(env["FIN_DEV_SOLANA_PERPS_TPSL_FILL_FILE"]).write_text(request, encoding="utf-8")
                    settled = call_pending(environment_name, ["status", operation_id, "--json"], env)
                    records[f"tpsl_{side}_settled"] = settled
                    if settled.get("status") != "completed" or not (settled.get("effects") or {}).get("proof_digest"):
                        raise AssertionError(f"The {side} TPSL did not settle from its exact keeper proof: {json.dumps(settled, sort_keys=True)}")
                    if network_fee_lamports(settled) != 15_000:
                        raise AssertionError(
                            "TPSL parent reconciliation must retain its 15,000-lamport placement fee and add no wallet-paid keeper fee."
                        )
                    actual_pnl = pending_request_pnl_deltas(settled)
                    if actual_pnl != [expected_signed_pnl]:
                        raise AssertionError(
                            f"The {side} TPSL settlement reported realized PnL {actual_pnl}, "
                            f"expected the exact signed USD-atom delta {expected_signed_pnl}."
                        )
                    pnl_rows = pending_request_pnl_rows(settled)
                    expected_position_kind = "derivative_long" if side == "long" else "derivative_short"
                    if (
                        len(pnl_rows) != 1
                        or (pnl_rows[0].get("asset") or {}).get("reference") != "jupiter:margin-usd"
                        or pnl_rows[0].get("position_kind") != expected_position_kind
                        or not pnl_rows[0].get("event_id")
                    ):
                        raise AssertionError(
                            f"The {side} TPSL PnL was not bound to its exact margin asset, derivative anchor, and event: "
                            f"{json.dumps(pnl_rows, sort_keys=True)}"
                        )
                    actual_credits = pending_request_credit_atoms(settled, USDC_MINT)
                    expected_credits = [] if expected_payout == "0" else [expected_payout]
                    if actual_credits != expected_credits:
                        raise AssertionError(
                            f"The {side} TPSL typed wallet proceeds {actual_credits} do not match "
                            f"the exact finalized payout {expected_credits}."
                        )
                    expected_tpsl_usdc = expected_tpsl_usdc - 11_000_000 + int(expected_payout)
                    closed = call(
                        environment_name,
                        ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                        env,
                    )
                    if any(item.get("position") == position for item in closed.get("positions", [])):
                        raise AssertionError("The TPSL keeper proof did not close the exact position.")
                    receive = call(
                        environment_name,
                        ["read", "--input", json.dumps(positions_input), "wallet.balances", "--json"],
                        env,
                    )
                    actual_usdc = wallet_token_amount(receive, USDC_MINT)
                    if actual_usdc != expected_tpsl_usdc:
                        raise AssertionError(
                            f"The {side} TPSL wallet cash differs from collateral, realized PnL, and fees: "
                            f"expected {expected_tpsl_usdc}, observed {actual_usdc}."
                        )
                    cases[side] = {
                        "request": request,
                        "trigger_type": request_type,
                        "realized_pnl_usd_atoms": actual_pnl[0],
                        "payout_atoms": expected_payout,
                        "open": summary(opened),
                        "placement": summary(placed),
                        "update": summary(updated),
                        "no_fill_cancel": summary(cancelled) if cancelled else None,
                        "cancelled_parent": summary(cancelled_parent) if cancelled_parent else None,
                        "settlement": summary(settled),
                        "receive_usdc_atoms_after_case": wallet_token_amount(receive, USDC_MINT),
                    }
                evidence["perps_tpsl"] = cases
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_limit_only:
                positions_input = {
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "wallet": "solana-rehearsal",
                }
                initial_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if initial_positions.get("positions"):
                    raise AssertionError("The isolated Perps limit rehearsal must start without positions.")
                initial_wallet = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "wallet.balances", "--json"],
                    env,
                )
                initial_usdc_atoms = wallet_token_amount(initial_wallet, USDC_MINT)
                initial_native_lamports = int(initial_wallet["native"]["amount_atoms"])
                request_input = {
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "wallet": "solana-rehearsal",
                    "market": "SOL",
                    "side": "long",
                    "funding_token": "USDC",
                    "collateral_amount_atoms": "100000",
                    "size_usd_atoms": "110000",
                    "trigger_price_usd_atoms": "150000000",
                    "trigger_above_threshold": True,
                    "maximum_protocol_fee_atoms": "100000",
                    "maximum_protocol_fee_usd_atoms": "100000",
                    "maximum_network_fee_lamports": 20_000,
                    "maximum_account_funding_lamports": "7273200",
                }
                create = call(
                    environment_name,
                    ["execute", "jupiter.perps.limit.create", "--input", json.dumps(request_input),
                     "--id", "solana-perps-limit-create", "--json"],
                    env,
                )
                records["perps_limit_create"] = create
                if not has_verified_open_limit_progress(create):
                    raise AssertionError(
                        "A placed Perps limit request must remain pending with verified open escrow and account-funding effects: "
                        + json.dumps(create, sort_keys=True)
                    )
                if not has_network_fee(create, "15000"):
                    raise AssertionError(
                        "Limit placement should retain the exact 15,000-lamport three-signature fee proof."
                    )
                create_operation_id = create.get("id")
                if not isinstance(create_operation_id, str) or not create_operation_id:
                    raise AssertionError("Perps limit placement omitted its durable operation UUID.")
                preview = create.get("preview") or {}
                binding = preview.get("binding") or create.get("binding") or {}
                request = binding.get("request") or (create.get("result") or {}).get("request")
                if not isinstance(request, str) or not request:
                    raise AssertionError(
                        "Perps limit placement omitted its exact request identity: "
                        + json.dumps(create, sort_keys=True)
                    )
                pending = call_pending(
                    environment_name, ["status", create_operation_id, "--json"], env
                )
                records["perps_limit_repeat_pending"] = pending
                if not has_verified_open_limit_progress(pending):
                    raise AssertionError(
                        "The same open limit request did not retain verified open escrow and funding on repeat reconcile: "
                        + json.dumps(pending, sort_keys=True)
                    )
                updated_trigger = "160000000"
                updated = call(
                    environment_name,
                    ["execute", "jupiter.perps.limit.update", "--input", json.dumps({
                        "network": "solana",
                        "cluster": "mainnet-beta",
                        "wallet": "solana-rehearsal",
                        "request": request,
                        "original_operation_id": create_operation_id,
                        "trigger_price_usd_atoms": updated_trigger,
                        "maximum_network_fee_lamports": "15000",
                    }), "--id", "solana-perps-limit-update", "--json"],
                    env,
                )
                records["perps_limit_update"] = updated
                if updated.get("status") != "completed" or not (updated.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        "Limit trigger update did not settle from its exact finalized instruction: "
                        + json.dumps(updated, sort_keys=True)
                    )
                still_pending = call_pending(
                    environment_name, ["status", create_operation_id, "--json"], env
                )
                records["perps_limit_parent_after_update"] = still_pending
                if still_pending.get("status") != "pending":
                    raise AssertionError(
                        "Updating a resting order must preserve its original pending escrow operation: "
                        + json.dumps(still_pending, sort_keys=True)
                    )
                open_snapshot = call(
                    environment_name,
                    ["read", "--input", json.dumps({
                        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal"
                    }), "jupiter.perps.positions", "--json"],
                    env,
                )
                open_requests = open_snapshot.get("pending_requests", [])
                observed_request = next(
                    (item for item in open_requests if item.get("request") == request), None
                )
                escrow = (observed_request or {}).get("escrow") or {}
                if (
                    observed_request is None
                    or observed_request.get("executed") is not False
                    or escrow.get("amount_atoms") != request_input["collateral_amount_atoms"]
                    or escrow.get("position")
                    != f"jupiter-perps-request:{request}"
                    or observed_request.get("trigger_price_usd_atoms") != updated_trigger
                ):
                    raise AssertionError(
                        "An open resting request must be listed with its exact locked escrow: "
                        + json.dumps(open_snapshot, sort_keys=True)
                    )
                # This different wallet action must pass through the engine
                # while the request's exact USDC escrow remains reserved.
                unrelated_wrap = call(
                    environment_name,
                    ["execute", "wallet.wrap", "--input", json.dumps({
                        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
                        "amount": "0.0001", "max_fee_sol": "0.00001",
                        "max_account_funding_sol": "0.003",
                        "requestId": "solana-perps-limit-unrelated-wrap",
                    }), "--id", "solana-perps-limit-unrelated-wrap", "--json"],
                    env,
                )
                records["perps_limit_unrelated_wrap"] = unrelated_wrap
                if unrelated_wrap.get("status") != "completed" or not has_network_fee(unrelated_wrap):
                    raise AssertionError(
                        "An unrelated wallet wrap should execute while exact Perps escrow remains locked: "
                        + json.dumps(unrelated_wrap, sort_keys=True)
                    )
                unrelated_unwrap = call(
                    environment_name,
                    ["execute", "wallet.unwrap", "--input", json.dumps({
                        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
                        "amount": "0.0001", "max_fee_sol": "0.00001",
                        "max_temporary_rent_sol": "0.003",
                        "requestId": "solana-perps-limit-unrelated-unwrap",
                    }), "--id", "solana-perps-limit-unrelated-unwrap", "--json"],
                    env,
                )
                records["perps_limit_unrelated_unwrap"] = unrelated_unwrap
                if unrelated_unwrap.get("status") != "completed" or not has_network_fee(unrelated_unwrap):
                    raise AssertionError(
                        "The unrelated wallet unwrap should settle while Perps escrow is pending: "
                        + json.dumps(unrelated_unwrap, sort_keys=True)
                    )
                state_before_restart = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                if len(state_before_restart.get("transactions", [])) != 4:
                    raise AssertionError(
                        "The placement, update and unrelated wrap/unwrap were not persisted exactly once."
                    )
                restart_fixture(environment_name, env)
                started = True
                after_restart = call_pending(
                    environment_name, ["status", create_operation_id, "--json"], env
                )
                records["perps_limit_restart_pending"] = after_restart
                if after_restart.get("status") != "pending":
                    raise AssertionError(
                        "The original immutable limit request did not remain pending after restart: "
                        + json.dumps(after_restart, sort_keys=True)
                    )
                Path(env["FIN_DEV_SOLANA_PERPS_LIMIT_FILL_FILE"]).write_text(request, encoding="utf-8")
                filled = call_pending(
                    environment_name, ["status", create_operation_id, "--json"], env
                )
                records["perps_limit_filled"] = filled
                if filled.get("status") != "completed" or not (filled.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        "The original limit operation did not settle from the exact finalized keeper proof: "
                        + json.dumps(filled, sort_keys=True)
                    )
                final_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps({
                        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal"
                    }), "jupiter.perps.positions", "--json"],
                    env,
                )
                finalized_position = next(
                    (p for p in final_positions.get("positions", [])
                     if p.get("position") == binding.get("position")),
                    None,
                )
                if (
                    finalized_position is None
                    or finalized_position.get("side") != "long"
                    or finalized_position.get("size_usd_atoms") != 110_000
                    or finalized_position.get("collateral_usd_atoms") != 99_000
                    or finalized_position.get("entry_price_usd_atoms") != int(updated_trigger)
                    or any(item.get("request") == request
                           for item in final_positions.get("pending_requests", []))
                ):
                    raise AssertionError(
                        "Finalized limit fill did not reveal the exact position, exposure, collateral, and trigger: "
                        + json.dumps(final_positions, sort_keys=True)
                    )
                before_cancel = call(
                    environment_name,
                    ["read", "--input", json.dumps({
                        "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"
                    }), "wallet.balances", "--json"],
                    env,
                )
                # Reuse the existing long position PDA. A cancel should return
                # only the temporary request/ATA rent; the original Position
                # PDA remains funded until the exact position is closed below.
                cancel_input = {**request_input, "side": "long"}
                cancel_create = call(
                    environment_name,
                    ["execute", "jupiter.perps.limit.create", "--input", json.dumps(cancel_input),
                     "--id", "solana-perps-limit-cancel-create", "--json"],
                    env,
                )
                records["perps_limit_cancel_create"] = cancel_create
                if not has_verified_open_limit_progress(cancel_create):
                    raise AssertionError(
                        "The second open limit request did not remain pending with verified escrow/funding before cancellation: "
                        + json.dumps(cancel_create, sort_keys=True)
                    )
                cancel_operation_id = cancel_create.get("id")
                cancel_binding = (cancel_create.get("preview") or {}).get("binding") or {}
                cancel_request = cancel_binding.get("request") or (cancel_create.get("result") or {}).get("request")
                if not isinstance(cancel_operation_id, str) or not isinstance(cancel_request, str):
                    raise AssertionError("The pending cancel scenario omitted its original durable request binding.")
                cancel = call(
                    environment_name,
                    ["execute", "jupiter.perps.limit.cancel", "--input", json.dumps({
                        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
                        "request": cancel_request, "original_operation_id": cancel_operation_id,
                        "maximum_network_fee_lamports": "5000",
                    }), "--id", "solana-perps-limit-cancel", "--json"],
                    env,
                )
                records["perps_limit_cancel"] = cancel
                if cancel.get("status") != "completed" or not (cancel.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        "The limit cancellation did not settle from its exact finalized close proof: "
                        + json.dumps(cancel, sort_keys=True)
                    )
                cancelled_parent = call_pending(
                    environment_name, ["status", cancel_operation_id, "--json"], env
                )
                records["perps_limit_cancelled_parent"] = cancelled_parent
                if cancelled_parent.get("status") != "completed" or not (cancelled_parent.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        "The original create did not settle its escrow/rent lifecycle from the cancel proof: "
                        + json.dumps(cancelled_parent, sort_keys=True)
                    )
                after_cancel = call(
                    environment_name,
                    ["read", "--input", json.dumps({
                        "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"
                    }), "wallet.balances", "--json"],
                    env,
                )
                if wallet_token_amount(after_cancel, USDC_MINT) != wallet_token_amount(before_cancel, USDC_MINT):
                    raise AssertionError("Limit cancellation did not return the exact escrow token amount.")

                close = call(
                    environment_name,
                    ["execute", "jupiter.perps.decrease_market", "--input", json.dumps({
                        **positions_input,
                        "position": binding.get("position"),
                        "receive_token": "USDC",
                        "size_usd_atoms": "110000",
                        "minimum_receive_atoms": "97957",
                        "maximum_protocol_fee_usd_atoms": "10000",
                        "maximum_slippage_bps": 50,
                        "maximum_network_fee_lamports": "10000",
                        "maximum_account_funding_lamports": "6000000",
                    }), "--id", "solana-perps-limit-close-filled-position", "--json"],
                    env,
                )
                records["perps_limit_close_filled_position"] = close
                if close.get("status") != "completed" or not (close.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        "The filled limit position did not close from its exact finalized market proof: "
                        + json.dumps(close, sort_keys=True)
                    )
                if not has_network_fee(close):
                    raise AssertionError("Closing the filled limit position omitted its exact network fee.")
                final_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if final_positions.get("positions") or final_positions.get("pending_requests"):
                    raise AssertionError(
                        "The limit rehearsal left an open position or request after cancel and close: "
                        + json.dumps(final_positions, sort_keys=True)
                    )
                final_wallet = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "wallet.balances", "--json"],
                    env,
                )
                final_usdc_atoms = wallet_token_amount(final_wallet, USDC_MINT)
                expected_usdc_atoms = initial_usdc_atoms - 1_550
                if final_usdc_atoms != expected_usdc_atoms:
                    raise AssertionError(
                        "Limit fill/cancel/close should restore USDC less the exact 1,000-atom opening "
                        "and 550-atom custody-bounded closing fees; "
                        f"expected {expected_usdc_atoms}, got {final_usdc_atoms}."
                    )
                network_fee_records = [create, updated, unrelated_wrap, unrelated_unwrap,
                                       cancel_create, cancel, close]
                expected_native_lamports = initial_native_lamports - sum(
                    network_fee_lamports(item) for item in network_fee_records
                )
                actual_native_lamports = int(final_wallet["native"]["amount_atoms"])
                if actual_native_lamports != expected_native_lamports:
                    raise AssertionError(
                        "Limit fill/cancel/close should restore native SOL and refundable funding less exact fees; "
                        f"expected {expected_native_lamports}, got {actual_native_lamports}."
                    )
                evidence["perps_limit"] = {
                    "request": request,
                    "operation_id": create_operation_id,
                    "placement": summary(create),
                    "repeat_pending": summary(pending),
                    "updated_trigger": summary(updated),
                    "parent_after_update": summary(still_pending),
                    "unrelated_wallet_action": [summary(unrelated_wrap), summary(unrelated_unwrap)],
                    "restart_pending": summary(after_restart),
                    "keeper_fill": summary(filled),
                    "position": binding.get("position"),
                    "cancel_placement": summary(cancel_create),
                    "cancel": summary(cancel),
                    "cancelled_original": summary(cancelled_parent),
                    "close_filled_position": summary(close),
                    "restored_usdc_atoms": final_usdc_atoms,
                    "net_network_fees_lamports": sum(
                        network_fee_lamports(item) for item in network_fee_records
                    ),
                    "fixture_transactions": len(json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", [])),
                }
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_only:
                positions_input = {
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "wallet": "solana-rehearsal",
                }
                initial_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if initial_positions.get("positions"):
                    raise AssertionError("The isolated Perps rehearsal must start without an open position.")
                perps_scenarios: dict[str, object] = {}
                for side in ("long", "short"):
                    scenario = f"perps-{side}"
                    increase_input = {
                        **positions_input,
                        "market": "SOL",
                        "side": side,
                        "funding_token": "USDC",
                        "collateral_amount_atoms": "11000000",
                        "size_usd_atoms": "12100000",
                        "maximum_protocol_fee_atoms": "5000000",
                        "maximum_protocol_fee_usd_atoms": "500000",
                        "maximum_slippage_bps": 50,
                        "maximum_network_fee_lamports": "10000",
                        "maximum_account_funding_lamports": "6000000",
                    }
                    increase = call(
                        environment_name,
                        ["execute", "jupiter.perps.increase_market", "--input", json.dumps(increase_input),
                         "--id", f"solana-{scenario}-increase", "--json"],
                        env,
                    )
                    records[f"{scenario}_increase"] = increase
                    if increase.get("status") != "completed":
                        raise AssertionError(f"Perps {side} increase did not settle: {json.dumps(increase, sort_keys=True)}")
                    opened_positions = call(
                        environment_name,
                        ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                        env,
                    )
                    new_positions = [
                        item for item in opened_positions.get("positions", [])
                        if item.get("position") not in {p.get("position") for p in initial_positions.get("positions", [])}
                    ]
                    if len(new_positions) != 1 or new_positions[0].get("side") != side:
                        raise AssertionError(
                            f"Could not discover exactly one {side} Perps position: "
                            + json.dumps(opened_positions, sort_keys=True)
                        )
                    position = new_positions[0]["position"]
                    decrease_input = {
                        **positions_input,
                        "position": position,
                        "receive_token": "USDC",
                        "size_usd_atoms": "12100000",
                        "minimum_receive_atoms": "10943010",
                        "maximum_protocol_fee_usd_atoms": "500000",
                        "maximum_slippage_bps": 50,
                        "maximum_network_fee_lamports": "10000",
                        "maximum_account_funding_lamports": "6000000",
                    }
                    decrease = call(
                        environment_name,
                        ["execute", "jupiter.perps.decrease_market", "--input", json.dumps(decrease_input),
                         "--id", f"solana-{scenario}-decrease", "--json"],
                        env,
                    )
                    records[f"{scenario}_decrease"] = decrease
                    if decrease.get("status") != "completed":
                        raise AssertionError(f"Perps {side} decrease did not settle: {json.dumps(decrease, sort_keys=True)}")
                    closed_positions = call(
                        environment_name,
                        ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                        env,
                    )
                    if any(p.get("position") == position for p in closed_positions.get("positions", [])):
                        raise AssertionError(f"The exact Perps {side} position remains open after finalized close proof.")
                    if closed_positions.get("positions"):
                        raise AssertionError(f"The Perps {side} position was not the only open exposure before the next case.")
                    perps_scenarios[side] = {
                        "position": position,
                        "increase": summary(increase),
                        "decrease": summary(decrease),
                        "size_usd_micros": "12100000",
                        "collateral_usdc_atoms": "11000000",
                        "closed_before_next_position": True,
                    }
                evidence["perps"] = perps_scenarios
                final_balances = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "wallet.balances", "--json"],
                    env,
                )
                expected_usdc = initial_usdc_atoms - 4_000
                actual_usdc = wallet_token_amount(final_balances, OUTPUT_MINT)
                if actual_usdc != expected_usdc:
                    raise AssertionError(
                        f"Perps round trips should restore USDC less four explicit 1,000-microUSD fees; "
                        f"expected {expected_usdc}, got {actual_usdc}."
                    )
                evidence["perps"]["net_usdc_after_four_fee_events"] = str(actual_usdc)
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if perps_collateral_only:
                positions_input = {
                    "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
                }
                initial_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if initial_positions.get("positions"):
                    raise AssertionError("The isolated collateral rehearsal must start without Perps positions.")
                open_input = {
                    **positions_input, "market": "SOL", "side": "long", "funding_token": "USDC",
                    "collateral_amount_atoms": "11000000", "size_usd_atoms": "12100000",
                    "maximum_protocol_fee_atoms": "5000000", "maximum_protocol_fee_usd_atoms": "500000",
                    "maximum_slippage_bps": 50, "maximum_network_fee_lamports": "10000",
                    "maximum_account_funding_lamports": "6000000",
                }
                opened = call(
                    environment_name,
                    ["execute", "jupiter.perps.increase_market", "--input", json.dumps(open_input),
                     "--id", "solana-perps-collateral-open", "--json"],
                    env,
                )
                records["perps_collateral_open"] = opened
                if (opened.get("status") != "completed" or not has_network_fee(opened)
                        or not has_perps_protocol_fee(opened)):
                    raise AssertionError("Standalone Perps position did not settle before collateral plan: "
                                         + json.dumps(opened, sort_keys=True))
                opened_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if len(opened_positions.get("positions", [])) != 1:
                    raise AssertionError("The open operation did not produce exactly one readable position.")
                position = opened_positions["positions"][0]["position"]
                if opened_positions["positions"][0].get("size_usd_atoms") != 12100000:
                    raise AssertionError("The initial position exposure differs from the approved notional.")

                plan_input = perps_collateral_plan(balance["wallet"], position)
                plan = json.loads(plan_input)
                preview = call(environment_name, ["plans", "preview", "--input", plan_input, "--valid-for", "10m", "--json"], env)
                if not preview.get("review_hash"):
                    raise AssertionError("The collateral plan preview omitted its review hash.")
                approval = call(
                    environment_name,
                    ["plans", "authorize", "--input", plan_input, "--id", "solana-perps-collateral-plan",
                     "--valid-for", "10m", "--review-hash", preview["review_hash"], "--json"],
                    env,
                )
                plan_id = approval.get("plan_id") or approval.get("id")
                if not plan_id:
                    raise AssertionError("The collateral plan did not return a durable plan ID.")
                completed_steps: list[dict] = []
                for step in plan["steps"]:
                    attempt = call(
                        environment_name,
                        ["execute", step["operation"], "--input", json.dumps(step["input"]),
                         "--id", f"solana-perps-collateral-{step['step_id']}", "--plan", plan_id,
                         "--step", step["step_id"], "--json"],
                        env,
                    )
                    records[f"perps_collateral_{step['step_id']}"] = attempt
                    if (attempt.get("status") != "completed"
                            or not (attempt.get("effects") or {}).get("proof_digest")
                            or not has_network_fee(attempt)
                            or not has_perps_protocol_fee(attempt)):
                        raise AssertionError(f"Collateral plan step {step['step_id']} did not settle: "
                                             + json.dumps(attempt, sort_keys=True))
                    completed_steps.append(attempt)

                close_input = {
                    **positions_input, "position": position, "receive_token": "USDC",
                    "size_usd_atoms": "12100000", "minimum_receive_atoms": "10943010",
                    "maximum_protocol_fee_usd_atoms": "500000", "maximum_slippage_bps": 50,
                    "maximum_network_fee_lamports": "10000", "maximum_account_funding_lamports": "6000000",
                }
                closed = call(
                    environment_name,
                    ["execute", "jupiter.perps.decrease_market", "--input", json.dumps(close_input),
                     "--id", "solana-perps-collateral-close", "--json"],
                    env,
                )
                records["perps_collateral_close"] = closed
                if (closed.get("status") != "completed"
                        or not (closed.get("effects") or {}).get("proof_digest")
                        or not has_network_fee(closed)
                        or not has_perps_protocol_fee(closed)):
                    raise AssertionError("Collateral test position did not close from finalized proof: "
                                         + json.dumps(closed, sort_keys=True))
                final_positions = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "jupiter.perps.positions", "--json"],
                    env,
                )
                if final_positions.get("positions"):
                    raise AssertionError("The collateral test position remains open after close settlement.")
                final_balances = call(
                    environment_name,
                    ["read", "--input", json.dumps(positions_input), "wallet.balances", "--json"],
                    env,
                )
                expected_usdc = initial_usdc_atoms - 4000
                if wallet_token_amount(final_balances, USDC_MINT) != expected_usdc:
                    raise AssertionError(
                        "Open/add/remove/close should restore USDC less four explicit 1,000-atom fees; "
                        f"expected {expected_usdc}, got {wallet_token_amount(final_balances, USDC_MINT)}."
                    )
                expected_native = initial_native_lamports - 20_000
                actual_native = int(final_balances["native"]["amount_atoms"])
                if actual_native != expected_native:
                    raise AssertionError(
                        "Open/add/remove/close should restore SOL less four exact 5,000-lamport network fees; "
                        f"expected {expected_native}, got {actual_native}."
                    )
                evidence["perps_collateral"] = {
                    "open": summary(opened), "plan_id": plan_id,
                    "named_plan_add_remove": [summary(item) for item in completed_steps],
                    "close": summary(closed), "position": position,
                    "restored_usdc_after_explicit_fees_atoms": str(expected_usdc),
                    "restored_native_after_explicit_fees_lamports": str(expected_native),
                    "final_position_count": 0,
                }
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            if jlp_only:
                def jlp_case(name: str, operation: str, token: str, amount: str, minimum: str) -> dict:
                    key = "deposit_token" if operation == "jupiter.jlp.mint" else "receive_token"
                    amount_key = "amount_atoms" if operation == "jupiter.jlp.mint" else "jlp_atoms"
                    minimum_key = "minimum_jlp_atoms" if operation == "jupiter.jlp.mint" else "minimum_receive_atoms"
                    request = {
                        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
                        key: token, amount_key: amount, minimum_key: minimum,
                        "maximum_network_fee_lamports": "10000", "maximum_account_funding_lamports": "0",
                    }
                    result = call(environment_name,
                        ["execute", operation, "--input", json.dumps(request), "--id", name, "--json"], env)
                    records[name] = result
                    if result.get("status") != "completed":
                        raise AssertionError(f"{name} did not settle: {json.dumps(result, sort_keys=True)}")
                    return result

                # Standalone USDC and native SOL round trips exercise the real
                # adapter through the public broker, engine and durable journal.
                usdc_mint = jlp_case("jlp-usdc-mint", "jupiter.jlp.mint", "USDC", "1000000", "990000")
                usdc_redeem = jlp_case("jlp-usdc-redeem", "jupiter.jlp.burn", "USDC", "1000000", "990000")
                sol_mint = jlp_case("jlp-sol-mint", "jupiter.jlp.mint", "SOL", "20000000", "1980000")
                sol_redeem = jlp_case("jlp-sol-redeem", "jupiter.jlp.burn", "SOL", "2000000", "19800000")

                plan_input = jlp_plan(balance["wallet"])
                plan_preview = call(environment_name, ["plans", "preview", "--input", plan_input, "--json"], env)
                if plan_preview.get("valid_for_seconds") != 900:
                    raise AssertionError("Two JLP steps should receive a 15-minute plan budget.")
                plan_approval = call(environment_name,
                    ["plans", "authorize", "--input", plan_input, "--id", "jlp-roundtrip-plan",
                     "--review-hash", plan_preview["review_hash"], "--json"], env)
                plan_id = plan_approval.get("plan_id") or plan_approval.get("id")
                if not plan_id:
                    raise AssertionError("The JLP named plan omitted its durable plan ID.")
                plan_mint = call(environment_name,
                    ["execute", "jupiter.jlp.mint", "--input", json.dumps(json.loads(plan_input)["steps"][0]["input"]),
                     "--id", "jlp-plan-mint-attempt", "--plan", plan_id, "--step", "mint", "--json"], env)
                records["jlp-plan-mint"] = plan_mint
                if plan_mint.get("status") != "completed":
                    raise AssertionError(f"The JLP plan mint did not settle: {json.dumps(plan_mint, sort_keys=True)}")
                plan_redeem = call(environment_name,
                    ["execute", "jupiter.jlp.burn", "--input", json.dumps(json.loads(plan_input)["steps"][1]["input"]),
                     "--id", "jlp-plan-redeem-attempt", "--plan", plan_id, "--step", "redeem", "--json"], env)
                records["jlp-plan-redeem"] = plan_redeem
                if plan_redeem.get("status") != "completed":
                    raise AssertionError(f"The JLP plan redeem did not settle: {json.dumps(plan_redeem, sort_keys=True)}")
                jlp_balances = call(
                    environment_name,
                    ["read", "--input", json.dumps({
                        "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"
                    }), "wallet.balances", "--json"],
                    env,
                )
                if wallet_token_amount(jlp_balances, USDC_MINT) != wallet_token_amount(balance, USDC_MINT):
                    raise AssertionError("JLP USDC mint/redeem scenarios did not conserve the original stablecoin balance.")
                if wallet_token_amount(jlp_balances, JLP_MINT) != wallet_token_amount(balance, JLP_MINT):
                    raise AssertionError("JLP mint/redeem scenarios left an unintended JLP balance.")
                for record in (usdc_mint, usdc_redeem, sol_mint, sol_redeem, plan_mint, plan_redeem):
                    if not has_network_fee(record):
                        raise AssertionError("A JLP action omitted its exact finalized network-fee cost.")
                evidence["jlp"] = {
                    "standalone_usdc_roundtrip": [summary(usdc_mint), summary(usdc_redeem)],
                    "standalone_native_roundtrip": [summary(sol_mint), summary(sol_redeem)],
                    "named_plan_roundtrip": [summary(plan_mint), summary(plan_redeem)],
                    "restored_token_balances_atoms": {
                        "usdc": str(wallet_token_amount(jlp_balances, USDC_MINT)),
                        "jlp": str(wallet_token_amount(jlp_balances, JLP_MINT)),
                    },
                }
                print(json.dumps(evidence, indent=2, sort_keys=True))
                return 0

            plan_input = future_funded_plan(balance["wallet"])
            plan_preview = call(environment_name, ["plans", "preview", "--input", plan_input, "--json"], env)
            if plan_preview.get("valid_for_seconds") != 900:
                raise AssertionError("Wrap/swap should receive a 15-minute plan budget.")
            if not plan_preview.get("review_hash"):
                raise AssertionError("Named plan preview omitted the review hash.")
            plan_approval = call(
                environment_name,
                ["plans", "authorize", "--input", plan_input, "--id", "solana-future-funded-plan",
                 "--review-hash", plan_preview["review_hash"], "--json"],
                env,
            )
            plan_id = plan_approval.get("plan_id") or plan_approval.get("id")
            if not plan_id:
                raise AssertionError("Named plan authorization omitted its durable plan ID.")
            plan_wrap = call(
                environment_name,
                ["execute", "wallet.wrap", "--input", json.dumps(json.loads(plan_input)["steps"][0]["input"]),
                 "--id", "solana-plan-wrap-attempt", "--plan", plan_id, "--step", "wrap", "--json"],
                env,
            )
            records["plan_wrap"] = plan_wrap
            if plan_wrap.get("status") != "completed":
                raise AssertionError(f"Named plan wrap did not settle: {json.dumps(plan_wrap, sort_keys=True)}")
            plan_swap = call(
                environment_name,
                ["execute", "jupiter.swap", "--input", json.dumps(json.loads(plan_input)["steps"][1]["input"]),
                 "--id", "solana-plan-swap-attempt", "--plan", plan_id, "--step", "swap", "--json"],
                env,
            )
            records["plan_swap"] = plan_swap
            if any(record.get("status") != "completed" for record in (plan_wrap, plan_swap)):
                raise AssertionError("The future-funded wrap→swap named plan did not settle both steps.")

            extra_wrap = call(
                environment_name,
                ["execute", "wallet.wrap", "--input", json.dumps({
                    "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
                    "amount": "0.02", "max_fee_sol": "0.00001", "requestId": "solana-extra-wrap",
                }, separators=(",", ":")), "--id", "solana-extra-wrap", "--json"],
                env,
            )
            records["extra_wrap"] = extra_wrap
            if extra_wrap.get("status") != "completed":
                raise AssertionError(
                    "The standalone wrap did not settle before spot swap scenarios: "
                    + json.dumps(extra_wrap, sort_keys=True)
                )
            unwrap = call(
                environment_name,
                ["execute", "wallet.unwrap", "--input", json.dumps({
                    "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta",
                    "amount": "0.02", "max_fee_sol": "0.00001", "max_temporary_rent_sol": "0.003",
                    "requestId": "solana-extra-unwrap",
                }, separators=(",", ":")), "--id", "solana-extra-unwrap", "--json"],
                env,
            )
            records["unwrap"] = unwrap
            if unwrap.get("status") != "completed" or not unwrap.get("effects", {}).get("proof_digest"):
                raise AssertionError(f"The partial unwrap did not settle from its closed temporary-account proof: {json.dumps(unwrap, sort_keys=True)}")
            native_plan_input = native_sol_roundtrip_plan(balance["wallet"])
            native_plan_preview = call(
                environment_name,
                ["plans", "preview", "--input", native_plan_input, "--json"],
                env,
            )
            if not native_plan_preview.get("review_hash"):
                raise AssertionError("Native SOL round-trip plan preview omitted its review hash.")
            native_plan_approval = call(
                environment_name,
                ["plans", "authorize", "--input", native_plan_input,
                 "--id", "solana-native-roundtrip-plan",
                 "--review-hash", native_plan_preview["review_hash"], "--json"],
                env,
            )
            native_plan_id = native_plan_approval.get("plan_id") or native_plan_approval.get("id")
            if not native_plan_id:
                raise AssertionError("Native SOL round-trip plan authorization omitted its durable plan ID.")
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            native_plan_in = call(
                environment_name,
                ["execute", "jupiter.swap", "--input",
                 json.dumps(json.loads(native_plan_input)["steps"][0]["input"]),
                 "--id", "solana-native-plan-in", "--plan", native_plan_id,
                 "--step", "native-in", "--json"],
                env,
            )
            records["native_plan_in"] = native_plan_in
            native_plan_out = call(
                environment_name,
                ["execute", "jupiter.swap", "--input",
                 json.dumps(json.loads(native_plan_input)["steps"][1]["input"]),
                 "--id", "solana-native-plan-out", "--plan", native_plan_id,
                 "--step", "native-out", "--json"],
                env,
            )
            records["native_plan_out"] = native_plan_out
            if any(record.get("status") != "completed" for record in (native_plan_in, native_plan_out)):
                raise AssertionError(
                    "The authorized Meta native-in→Router native-out plan did not settle both legs: "
                    + json.dumps([native_plan_in, native_plan_out], sort_keys=True)
                )

            # A public preview informs the human, but does not pin the router
            # for a later named-plan execution. The second candidate is a real
            # synthetic JupiterZ Fill instruction, including its maker signer,
            # native WSOL account creation, and same-transaction close.
            route_switch_input = json.loads(
                swap_input("meta_aggregator", "20000000", "1990000", "native", OUTPUT_MINT)
            )
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            preview_metis = call(
                environment_name,
                ["preview", "jupiter.swap", "--input", json.dumps(route_switch_input), "--json"],
                env,
            )
            if prepared_binding(preview_metis).get("router") != "metis":
                raise AssertionError("The first public preview did not return the expected Metis candidate.")
            route_switch_plan = exact_swap_plan(
                balance["wallet"], "Synthetic router-flexible native swap", "swap", route_switch_input
            )
            route_switch_preview = call(
                environment_name, ["plans", "preview", "--input", route_switch_plan, "--json"], env
            )
            route_switch_approval = call(
                environment_name,
                ["plans", "authorize", "--input", route_switch_plan, "--id",
                 "solana-router-flexible-native-plan", "--review-hash",
                 route_switch_preview["review_hash"], "--json"],
                env,
            )
            route_switch_id = route_switch_approval.get("plan_id") or route_switch_approval.get("id")
            if not route_switch_id:
                raise AssertionError("Router-flexible named plan omitted its durable plan ID.")
            # Exact authority binds the economic input. Tampered spend,
            # recipient mint, and minimum output must all fail before submit.
            for suffix, field, value in (
                ("spend", "amount_atoms", "20000001"),
                ("recipient", "output_mint", PYUSD_MINT),
                ("floor", "minimum_output_atoms", "1989999"),
            ):
                tampered = dict(route_switch_input)
                tampered[field] = value
                rejected = call_pending(
                    environment_name,
                    ["execute", "jupiter.swap", "--input", json.dumps(tampered),
                     "--id", f"solana-router-flexible-tampered-{suffix}", "--plan", route_switch_id,
                     "--step", "swap", "--json"],
                    env,
                )
                rejection = rejected.get("error", {})
                if rejection.get("code") != "PLAN_STEP_SCOPE_MISMATCH":
                    raise AssertionError(
                        f"Tampered {suffix} was not rejected by the exact named-plan binding: "
                        + json.dumps(rejected, sort_keys=True)
                    )
            Path(env["FIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE"]).write_text("jupiterz")
            switch_rpc_before = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get(
                "rpc_method_counts", {}
            )
            route_switch_native = call(
                environment_name,
                ["execute", "jupiter.swap", "--input", json.dumps(route_switch_input),
                 "--id", "solana-router-flexible-native", "--plan", route_switch_id,
                 "--step", "swap", "--json"],
                env,
            )
            records["route_switch_native"] = route_switch_native
            if route_switch_native.get("status") != "completed":
                raise AssertionError(
                    "Exact named-plan execution did not accept the fresh JupiterZ candidate after Metis preview: "
                    + json.dumps(route_switch_native, sort_keys=True)
                )
            route_binding = prepared_binding(route_switch_native)
            if route_binding.get("router") != "jupiterz":
                raise AssertionError("Execution reused the preview router instead of the fresh JupiterZ candidate.")
            if route_binding.get("fee_payer") != ORDER_ENGINE_MAKER:
                raise AssertionError("JupiterZ did not retain the remote maker as network-fee sponsor.")
            if not any(
                account.get("payer") == balance["wallet"]
                and account.get("refund_authority") == balance["wallet"]
                and int(account.get("lamports", 0)) == 2_039_280
                for account in route_binding.get("funding_accounts", [])
            ):
                raise AssertionError("Sponsored JupiterZ did not fund the wallet-owned temporary ATA from the wallet.")
            if conversion_atoms(route_switch_native) != ("20000000", "2000000"):
                raise AssertionError("Fresh JupiterZ settlement did not prove the approved native principal and output.")
            if account_funding_deltas(route_switch_native) or cash_debit_atoms(route_switch_native) != ["0"]:
                raise AssertionError("Fresh JupiterZ native create+close did not settle as exactly zero net rent.")
            sponsored_evidence = (route_switch_native.get("effects") or {}).get("evidence") or {}
            sponsored_proof = sponsored_evidence.get("proof") or {}
            if sponsored_evidence.get("network_fee_lamports") != 5_000 or sponsored_proof.get("fee_lamports") != 5_000:
                raise AssertionError("JupiterZ finalized evidence omitted the 5,000-lamport sponsor-paid network fee.")
            proof_keys = sponsored_proof.get("account_keys", [])
            sponsor_index = proof_keys.index(ORDER_ENGINE_MAKER)
            sponsor_pre = sponsored_proof["pre_balances_lamports"][sponsor_index]
            sponsor_post = sponsored_proof["post_balances_lamports"][sponsor_index]
            if sponsor_pre - sponsor_post != 5_000:
                raise AssertionError("JupiterZ finalized account balances did not charge 5,000 lamports to its sponsor.")
            route_proof = route_switch_native.get("settlement_proof", route_switch_native.get("effects", {}))
            route_tx = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())["transactions"][-1]
            route_wire = base64.b64decode(route_tx["wire"])
            if base58_bytes(ORDER_ENGINE_PROGRAM) not in route_wire:
                raise AssertionError("Synthetic JupiterZ settlement transaction omitted the actual OrderEngine program key.")
            switch_rpc_after = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get(
                "rpc_method_counts", {}
            )
            rpc_delta = {
                method: count - switch_rpc_before.get(method, 0)
                for method, count in switch_rpc_after.items()
                if count - switch_rpc_before.get(method, 0)
            }
            # Whole execute includes one candidate review and the final
            # dispatch guard: one simulation, a review fee plus fresh fee, and
            # one current-height check. A second simulation means the frozen
            # review was repeated inside submit_revalidated.
            if (
                rpc_delta.get("getFeeForMessage") != 2
                or rpc_delta.get("getBlockHeight") != 1
                or rpc_delta.get("simulateTransaction") != 1
            ):
                raise AssertionError(f"Unexpected cached-review/dispatch RPC counts: {rpc_delta}")

            # OrderEngine also supports the IDL's optional native endpoints.
            # This named plan exercises both directions directly: there is no
            # WSOL ATA creation, close, or rent movement on either leg.
            direct_native_in_input = json.loads(
                swap_input("meta_aggregator", "20000000", "1990000", "native", OUTPUT_MINT)
            )
            direct_native_out_input = json.loads(
                swap_input("meta_aggregator", "2000000", "19900000", OUTPUT_MINT, "native")
            )
            direct_native_plan = exact_swap_roundtrip_plan(
                balance["wallet"], "Synthetic JupiterZ direct-native roundtrip",
                direct_native_in_input, direct_native_out_input,
            )
            direct_native_plan_preview = call(
                environment_name,
                ["plans", "preview", "--input", direct_native_plan, "--json"],
                env,
            )
            direct_native_approval = call(
                environment_name,
                ["plans", "authorize", "--input", direct_native_plan,
                 "--id", "solana-jupiterz-direct-native-plan",
                 "--review-hash", direct_native_plan_preview["review_hash"], "--json"],
                env,
            )
            direct_native_id = direct_native_approval.get("plan_id") or direct_native_approval.get("id")
            if not direct_native_id:
                raise AssertionError("Direct-native JupiterZ plan omitted its durable ID.")
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            Path(env["FIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE"]).write_text("jupiterz_native_input")
            direct_native_in = call(
                environment_name,
                ["execute", "jupiter.swap", "--input", json.dumps(direct_native_in_input),
                 "--id", "solana-jupiterz-direct-native-in", "--plan", direct_native_id,
                 "--step", "convert-out", "--json"],
                env,
            )
            records["jupiterz_direct_native_in"] = direct_native_in
            Path(env["FIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE"]).write_text("jupiterz_native_output")
            direct_native_out = call(
                environment_name,
                ["execute", "jupiter.swap", "--input", json.dumps(direct_native_out_input),
                 "--id", "solana-jupiterz-direct-native-out", "--plan", direct_native_id,
                 "--step", "convert-back", "--json"],
                env,
            )
            records["jupiterz_direct_native_out"] = direct_native_out
            for label, record, native_side, expected_conversion, expected_wallet_delta in (
                ("input", direct_native_in, "input", ("20000000", "2000000"), -20_000_000),
                ("output", direct_native_out, "output", ("2000000", "20000000"), 20_000_000),
            ):
                if record.get("status") != "completed":
                    raise AssertionError(f"JupiterZ direct-native {label} step did not settle: {json.dumps(record, sort_keys=True)}")
                binding = prepared_binding(record)
                if binding.get("fee_payer") != ORDER_ENGINE_MAKER:
                    raise AssertionError(f"JupiterZ direct-native {label} did not use its remote maker fee payer.")
                direct_flag = f"rfq_native_{native_side}_direct"
                if binding.get(direct_flag) is not True:
                    raise AssertionError(f"JupiterZ direct-native {label} binding omitted {direct_flag}.")
                if binding.get("account_funding_lamports") != 0 or binding.get("funding_accounts"):
                    raise AssertionError(f"JupiterZ direct-native {label} unexpectedly funded a temporary account.")
                if binding.get("closed_wrapped_native_accounts"):
                    raise AssertionError(f"JupiterZ direct-native {label} unexpectedly closed a WSOL account.")
                if conversion_atoms(record) != expected_conversion:
                    raise AssertionError(f"JupiterZ direct-native {label} did not prove exact approved quantities.")
                effects = record.get("effects", {}).get("effects", [])
                if any(effect.get("kind") == "cash_debit" and int((effect.get("amount") or {}).get("atoms", "0")) != 0 for effect in effects):
                    raise AssertionError(f"JupiterZ direct-native {label} charged wallet cash for sponsored fee or rent.")
                evidence = record.get("effects", {}).get("evidence", {})
                proof = evidence.get("proof", {})
                if evidence.get("network_fee_lamports") != 5_000 or proof.get("fee_lamports") != 5_000:
                    raise AssertionError(f"JupiterZ direct-native {label} omitted its 5,000-lamport sponsor fee proof.")
                program_instruction = next(
                    (instruction for instruction in proof.get("message_instructions", [])
                     if instruction.get("program_id") == ORDER_ENGINE_PROGRAM),
                    None,
                )
                sentinel_index = 2 if native_side == "input" else 4
                sentinel = (program_instruction or {}).get("accounts", [])[sentinel_index]
                if sentinel.get("address") != ORDER_ENGINE_PROGRAM or sentinel.get("signer") or sentinel.get("writable"):
                    raise AssertionError(f"JupiterZ direct-native {label} did not encode the readonly optional-account sentinel.")
                keys = proof.get("account_keys", [])
                wallet_index = keys.index(balance["wallet"])
                wallet_delta = proof["post_balances_lamports"][wallet_index] - proof["pre_balances_lamports"][wallet_index]
                if wallet_delta != expected_wallet_delta:
                    raise AssertionError(f"JupiterZ direct-native {label} wallet SOL delta included fee or rent: {wallet_delta}.")

            # Exercise the reverse router transition on a token-only return:
            # preview JupiterZ, then execute the same exact plan against Metis.
            reverse_input = json.loads(swap_input("meta_aggregator", "2000000", "1990000", PYUSD_MINT, OUTPUT_MINT))
            reverse_back_input = json.loads(
                swap_input("meta_aggregator", "2000000", "1990000", OUTPUT_MINT, PYUSD_MINT)
            )
            reverse_plan = exact_swap_roundtrip_plan(
                balance["wallet"], "Synthetic reverse router-flexible swap", reverse_input, reverse_back_input
            )
            reverse_preview = call(
                environment_name,
                ["plans", "preview", "--input", reverse_plan, "--json"],
                env,
            )
            reverse_approval = call(
                environment_name,
                ["plans", "authorize", "--input", reverse_plan, "--id",
                 "solana-router-flexible-reverse-plan", "--review-hash", reverse_preview["review_hash"], "--json"],
                env,
            )
            reverse_id = reverse_approval.get("plan_id") or reverse_approval.get("id")
            Path(env["FIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE"]).write_text("jupiterz")
            preview_jupiterz = call(
                environment_name,
                ["preview", "jupiter.swap", "--input", json.dumps(reverse_input), "--json"],
                env,
            )
            if prepared_binding(preview_jupiterz).get("router") != "jupiterz":
                raise AssertionError("The reverse public preview did not return JupiterZ.")
            Path(env["FIN_DEV_SOLANA_FIXTURE_NEXT_META_ROUTER_FILE"]).write_text("metis")
            route_switch_metis = call(
                environment_name,
                ["execute", "jupiter.swap", "--input", json.dumps(reverse_input),
                 "--id", "solana-router-flexible-metis", "--plan", reverse_id,
                 "--step", "convert-out", "--json"],
                env,
            )
            records["route_switch_metis"] = route_switch_metis
            if route_switch_metis.get("status") != "completed" or prepared_binding(route_switch_metis).get("router") != "metis":
                raise AssertionError(
                    "Exact named-plan execution did not accept the fresh Metis candidate after JupiterZ preview: "
                    + json.dumps(route_switch_metis, sort_keys=True)
                )
            metis_tx = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())["transactions"][-1]
            if base58_bytes(JUPITER_V6_PROGRAM) not in base64.b64decode(metis_tx["wire"]):
                raise AssertionError("Fresh Metis settlement transaction omitted the Jupiter v6 program key.")
            route_switch_return = call(
                environment_name,
                ["execute", "jupiter.swap", "--input", json.dumps(reverse_back_input),
                 "--id", "solana-router-flexible-return", "--plan", reverse_id,
                 "--step", "convert-back", "--json"],
                env,
            )
            records["route_switch_return"] = route_switch_return
            if route_switch_return.get("status") != "completed":
                raise AssertionError("Reverse route-switch roundtrip did not complete its return leg.")
            if conversion_atoms(route_switch_return) != ("2000000", "2000000"):
                raise AssertionError("Reverse route-switch return did not settle its exact approved quantities.")
            # Both native directions begin without a user WSOL ATA. Native
            # output must create and close one just like the native-input path.
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            router_native_out = call(
                environment_name,
                ["execute", "--input", swap_input("router", "2000000", "19900000", OUTPUT_MINT, "native"),
                 "--id", "solana-router-native-out", "--json", "jupiter.swap"],
                env,
            )
            records["router_native_out"] = router_native_out
            if router_native_out.get("status") != "completed":
                raise AssertionError(
                    "Router USDC→native SOL swap did not settle: "
                    + json.dumps(router_native_out, sort_keys=True)
                )
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            meta_native_out = call(
                environment_name,
                ["execute", "--input", swap_input("meta_aggregator", "2000000", "19900000", OUTPUT_MINT, "native"),
                 "--id", "solana-meta-native-out", "--json", "jupiter.swap"],
                env,
            )
            records["meta_native_out"] = meta_native_out
            if meta_native_out.get("status") != "completed":
                raise AssertionError(
                    "Meta USDC→native SOL swap did not settle: "
                    + json.dumps(meta_native_out, sort_keys=True)
                )
            # Exercise the real native-input transaction shape: the user's WSOL
            # ATA is absent, so Jupiter creates it, funds it from wallet SOL,
            # swaps the exact amount, and closes it in the same transaction.
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            router_native_in = call(
                environment_name,
                ["execute", "--input", swap_input("router", "20000000", "1990000", "native"),
                 "--id", "solana-router-native-in", "--json", "jupiter.swap"],
                env,
            )
            records["router_native_in"] = router_native_in
            if router_native_in.get("status") != "completed":
                raise AssertionError(
                    "Router native SOL→USDC swap did not settle with a temporary WSOL ATA: "
                    + json.dumps(router_native_in, sort_keys=True)
                )
            Path(env["FIN_DEV_SOLANA_FIXTURE_REMOVE_WSOL_ATA_FILE"]).touch()
            meta_native_in = call(
                environment_name,
                ["execute", "--input", swap_input("meta_aggregator", "20000000", "1990000", "native", OUTPUT_MINT),
                 "--id", "solana-meta-native-in", "--json", "jupiter.swap"],
                env,
            )
            records["meta_native_in"] = meta_native_in
            if meta_native_in.get("status") != "completed":
                raise AssertionError(
                    "Meta native SOL→USDC swap did not settle with a temporary WSOL ATA: "
                    + json.dumps(meta_native_in, sort_keys=True)
                )
            pyusd_router_in = call(
                environment_name,
                ["execute", "--input", swap_input("router", "2000000", "1990000", PYUSD_MINT, OUTPUT_MINT),
                 "--id", "solana-pyusd-router-in", "--json", "jupiter.swap"],
                env,
            )
            records["pyusd_router_in"] = pyusd_router_in
            if pyusd_router_in.get("status") != "completed":
                raise AssertionError(
                    "Token-2022 PYUSD→USDC Router swap did not settle: "
                    + json.dumps(pyusd_router_in, sort_keys=True)
                )
            pyusd_router_out = call(
                environment_name,
                ["execute", "--input", swap_input("router", "2000000", "1990000", OUTPUT_MINT, PYUSD_MINT),
                 "--id", "solana-pyusd-router-out", "--json", "jupiter.swap"],
                env,
            )
            records["pyusd_router_out"] = pyusd_router_out
            if pyusd_router_out.get("status") != "completed":
                raise AssertionError(
                    "USDC→Token-2022 PYUSD Router swap did not settle: "
                    + json.dumps(pyusd_router_out, sort_keys=True)
                )
            pyusd_meta_in = call(
                environment_name,
                ["execute", "--input", swap_input("meta_aggregator", "2000000", "1990000", OUTPUT_MINT, PYUSD_MINT),
                 "--id", "solana-pyusd-meta-in", "--json", "jupiter.swap"],
                env,
            )
            records["pyusd_meta_in"] = pyusd_meta_in
            if pyusd_meta_in.get("status") != "completed":
                raise AssertionError(
                    "USDC→Token-2022 PYUSD Meta swap did not settle: "
                    + json.dumps(pyusd_meta_in, sort_keys=True)
                )
            pyusd_meta_out = call(
                environment_name,
                ["execute", "--input", swap_input("meta_aggregator", "2000000", "1990000", PYUSD_MINT, OUTPUT_MINT),
                 "--id", "solana-pyusd-meta-out", "--json", "jupiter.swap"],
                env,
            )
            records["pyusd_meta_out"] = pyusd_meta_out
            if pyusd_meta_out.get("status") != "completed":
                raise AssertionError(
                    "Token-2022 PYUSD→USDC Meta swap did not settle: "
                    + json.dumps(pyusd_meta_out, sort_keys=True)
                )
            restored_balances = call(
                environment_name,
                ["read", "--input", json.dumps({
                    "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"
                }), "wallet.balances", "--json"],
                env,
            )
            if wallet_token_amount(restored_balances, PYUSD_MINT) != initial_pyusd_atoms:
                raise AssertionError("The PYUSD round trip did not restore its original token balance.")
            expected_usdc_after_plan = initial_usdc_atoms + 4_000_000
            actual_usdc_after_plan = wallet_token_amount(restored_balances, OUTPUT_MINT)
            if actual_usdc_after_plan != expected_usdc_after_plan:
                raise AssertionError(
                    "The USDC round trips did not restore their original token balance: "
                    f"initial={initial_usdc_atoms}, expected_after_plan={expected_usdc_after_plan}, "
                    f"snapshot={actual_usdc_after_plan}."
                )
            for label, record, expected_spent, expected_received in (
                ("Router native input", router_native_in, "20000000", "2000000"),
                ("Meta native input", meta_native_in, "20000000", "2000000"),
                ("Router native output", router_native_out, "2000000", "20000000"),
                ("Meta native output", meta_native_out, "2000000", "20000000"),
                ("Named-plan Meta native input", native_plan_in, "20000000", "2000000"),
                ("Named-plan Router native output", native_plan_out, "2000000", "20000000"),
            ):
                if account_funding_deltas(record):
                    raise AssertionError(f"{label} retained a rent position after its temporary ATA closed.")
                if cash_debit_atoms(record) != ["0"]:
                    raise AssertionError(f"{label} did not emit exactly the approved zero-net-rent debit.")
                if not has_network_fee(record) or network_fee_lamports(record) != 5_000:
                    raise AssertionError(f"{label} did not settle the exact 5,000-lamport network fee.")
                if conversion_atoms(record) != (expected_spent, expected_received):
                    raise AssertionError(
                        f"{label} did not prove exact principal consumption and output: "
                        f"{conversion_atoms(record)}."
                    )
            for record in (
                router_native_in, router_native_out, meta_native_in, meta_native_out,
                pyusd_router_in, pyusd_router_out, pyusd_meta_in, pyusd_meta_out,
            ):
                if not has_network_fee(record):
                    raise AssertionError("A spot swap omitted its exact finalized network-fee cost.")
            transfer_ack = call_pending(
                environment_name,
                ["execute", "--input", json.dumps({
                    "wallet": "solana-rehearsal",
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "recipient": "YMN9Qj5jPNp7j14VPcML1B6xGgcPWVZUGLFU3Mnyfaf",
                    "amount": "0.0001",
                    "max_fee_sol": "0.00001",
                    "requestId": "solana-native-transfer-rehearsal",
                }, separators=(",", ":")), "--id", "solana-native-transfer-rehearsal", "--json", "wallet.transfer"],
                env,
            )
            records["native_transfer_lost_response"] = transfer_ack
            if transfer_ack.get("status") not in {"unknown", "submitted", "pending"}:
                raise AssertionError(
                    "The deliberately lost send response did not remain unresolved: "
                    + json.dumps(transfer_ack, sort_keys=True)
                )
            transfer_operation_id = transfer_ack.get("id")
            if not isinstance(transfer_operation_id, str) or not transfer_operation_id:
                raise AssertionError(
                    "The unresolved transfer response omitted its durable operation UUID: "
                    + json.dumps(transfer_ack, sort_keys=True)
                )
            fixture_before_restart = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            if not fixture_before_restart.get("lost_response_consumed"):
                raise AssertionError("The synthetic RPC did not persist the accepted-but-unacknowledged transfer.")
            all_completed = (
                plan_wrap, plan_swap, extra_wrap, unwrap,
                router_native_in, router_native_out, meta_native_in, meta_native_out,
                pyusd_router_in, pyusd_router_out, pyusd_meta_in, pyusd_meta_out,
                native_plan_in, native_plan_out,
                route_switch_native, route_switch_metis, route_switch_return,
                direct_native_in, direct_native_out,
            )
            if len(fixture_before_restart.get("transactions", [])) != 20:
                raise AssertionError("The fixture did not record each planned transaction exactly once.")
            for record in all_completed:
                if record.get("status") != "completed" or not record.get("effects", {}).get("proof_digest"):
                    raise AssertionError(f"Swap did not settle from finalized proof: {record.get('error')}")
                if record.get("input", {}).get("cluster") != "mainnet-beta":
                    raise AssertionError("A Solana write omitted the reviewed mainnet-beta cluster.")
                if record.get("input", {}).get("maximum_slippage_bps", 0) > 50:
                    raise AssertionError("Swap exceeded the reviewed cluster or slippage fixture bounds.")

            stop = subprocess.run(
                [sys.executable, str(DEV), "stop", environment_name],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
                timeout=30,
            )
            if stop.returncode:
                raise RuntimeError(stop.stderr or stop.stdout or "Synthetic environment stop failed.")
            started = False
            restart = subprocess.run(
                [sys.executable, str(DEV), "start", environment_name,
                 "--mode", "synthetic", "--fixture", "solana"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
                timeout=180,
            )
            if restart.returncode:
                raise RuntimeError(restart.stderr or restart.stdout or "Synthetic fixture restart failed.")
            started = True
            recovered = [
                call(environment_name, ["status", record["id"], "--json"], env)
                for record in all_completed
            ]
            recovered_transfer = call(
                environment_name,
                ["status", transfer_operation_id, "--json"],
                env,
            )
            if recovered_transfer.get("status") != "completed" or not recovered_transfer.get("effects", {}).get("proof_digest"):
                raise AssertionError("The accepted transfer did not reconcile after restart from its original proof.")
            if not str((recovered_transfer.get("submission") or {}).get("reference", "")).startswith("solana:message:"):
                raise AssertionError("Recovered transfer lost its canonical immutable message identity.")
            fixture_after_restart = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
            if len(fixture_after_restart.get("transactions", [])) != len(fixture_before_restart["transactions"]):
                raise AssertionError("Restart reconciliation broadcast a duplicate financial transaction.")

            swap_recoveries: dict[str, dict] = {}
            for label, route, input_mint, output_mint in (
                ("router", "router", OUTPUT_MINT, PYUSD_MINT),
                ("meta", "meta_aggregator", PYUSD_MINT, OUTPUT_MINT),
            ):
                operation_id = f"solana-{label}-lost-response-recovery"
                request = swap_input(route, "1000000", "995000", input_mint, output_mint)
                pending = call_pending(
                    environment_name,
                    ["execute", "--input", request, "--id", operation_id, "--json", "jupiter.swap"],
                    env,
                )
                if pending.get("status") not in {"unknown", "submitted", "pending"}:
                    raise AssertionError(
                        f"The {label} accepted-but-unacknowledged swap was not left unresolved: "
                        + json.dumps(pending, sort_keys=True)
                    )
                durable_operation_id = pending.get("id")
                if not isinstance(durable_operation_id, str) or not durable_operation_id:
                    raise AssertionError(
                        f"The {label} unresolved response omitted its durable operation UUID: "
                        + json.dumps(pending, sort_keys=True)
                    )
                records[f"{label}_lost_response"] = pending
                pending_reference = (pending.get("submission") or {}).get("reference")
                if not str(pending_reference).startswith("solana:message:"):
                    raise AssertionError(f"The {label} uncertain swap lost its canonical message identity.")
                before_duplicate = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                duplicate = call_pending(
                    environment_name,
                    ["execute", "--input", request, "--id", operation_id, "--json", "jupiter.swap"],
                    env,
                )
                if duplicate.get("id") != durable_operation_id:
                    raise AssertionError(f"The {label} duplicate request changed its durable operation UUID.")
                if (duplicate.get("submission") or {}).get("reference") != pending_reference:
                    raise AssertionError(f"The {label} duplicate attempt changed the immutable transaction identity.")
                after_duplicate = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                if len(after_duplicate.get("transactions", [])) != len(before_duplicate.get("transactions", [])):
                    raise AssertionError(f"The {label} duplicate attempt broadcast a second transaction.")
                restart_fixture(environment_name, env)
                recovered_swap = call(
                    environment_name, ["status", durable_operation_id, "--json"], env
                )
                if recovered_swap.get("status") != "completed" or not (recovered_swap.get("effects") or {}).get("proof_digest"):
                    raise AssertionError(
                        f"The {label} swap did not reconcile from its original finalized proof after restart: "
                        + json.dumps(recovered_swap, sort_keys=True)
                    )
                if (recovered_swap.get("submission") or {}).get("reference") != pending_reference:
                    raise AssertionError(f"The {label} recovery changed its original message identity.")
                after_recovery = json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text())
                if len(after_recovery.get("transactions", [])) != len(after_duplicate.get("transactions", [])):
                    raise AssertionError(f"The {label} recovery broadcast a duplicate transaction.")
                swap_recoveries[label] = summary(recovered_swap)
                records[f"{label}_recovery"] = recovered_swap

            final_balances = call(
                environment_name,
                ["read", "--input", json.dumps({
                    "wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"
                }), "wallet.balances", "--json"],
                env,
            )
            if wallet_token_amount(final_balances, PYUSD_MINT) != initial_pyusd_atoms:
                raise AssertionError("Router/Meta lost-response round trip did not restore PYUSD holdings.")
            if wallet_token_amount(final_balances, OUTPUT_MINT) != expected_usdc_after_plan:
                raise AssertionError("Router/Meta lost-response round trip did not restore USDC holdings.")
            expected = [summary(record) for record in all_completed]
            actual = [summary(record) for record in recovered]
            if actual != expected:
                raise AssertionError("Restarted status changed a completed operation identity or proof.")
            evidence["snapshot"] = {
                "cluster": balance["cluster"],
                "coverage_kind": balance.get("coverage", {}).get("kind"),
            }
            evidence["named_plan"] = {
                "plan_id": plan_id,
                "steps": actual[:2],
                "funding_dependency": "verified wrapped SOL from wrap step to swap principal",
            }
            evidence["standalone_wrap"] = actual[2]
            evidence["standalone_unwrap"] = actual[3]
            evidence["native_sol_round_trips"] = {
                "router": actual[4:6],
                "meta": actual[6:8],
            }
            evidence["native_sol_named_plan_roundtrip"] = actual[12:14]
            evidence["router_flexible_named_plans"] = {
                "preview_metis_execute_jupiterz": actual[14],
                "preview_jupiterz_execute_metis_and_return": actual[15:17],
                "tampered_spend_recipient_floor_rejected": True,
                "execute_rpc_counts": rpc_delta,
            }
            evidence["jupiterz_direct_native_named_plan"] = actual[17:19]
            evidence["pyusd_token_2022_round_trips"] = {
                "router": actual[8:10],
                "meta": actual[10:12],
            }
            evidence["restored_token_balances_atoms"] = {
                "pyusd": str(wallet_token_amount(restored_balances, PYUSD_MINT)),
                "usdc": str(wallet_token_amount(restored_balances, OUTPUT_MINT)),
            }
            evidence["native_transfer"] = summary(recovered_transfer)
            evidence["swap_lost_response_recovery"] = swap_recoveries
            evidence["post_recovery_token_balances_atoms"] = {
                "pyusd": str(wallet_token_amount(final_balances, PYUSD_MINT)),
                "usdc": str(wallet_token_amount(final_balances, OUTPUT_MINT)),
            }
            evidence["lost_response_recovered_after_restart"] = True
            print(json.dumps(evidence, indent=2, sort_keys=True))
            return 0
        finally:
            failure = sys.exc_info()[1]
            stop_result = None
            if started:
                stop_result = subprocess.run(
                    [sys.executable, str(DEV), "stop", environment_name],
                    cwd=REPO,
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
            if failure is not None:
                failure_dir = Path(tempfile.mkdtemp(prefix="fin-solana-rehearsal-failure-"))
                ledger = Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"])
                if ledger.exists():
                    shutil.copyfile(ledger, failure_dir / "fixture-ledger.json")
                (failure_dir / "operations.json").write_text(
                    json.dumps(records, indent=2, sort_keys=True), encoding="utf-8"
                )
                (failure_dir / "failure.txt").write_text(
                    str(failure), encoding="utf-8"
                )
                if stop_result is not None:
                    (failure_dir / "stop-output.txt").write_text(
                        stop_result.stderr or stop_result.stdout, encoding="utf-8"
                    )
                broker_logs = sorted(
                    state_home.rglob("broker-*.log"), key=lambda path: path.stat().st_mtime
                )
                if broker_logs:
                    shutil.copyfile(broker_logs[-1], failure_dir / "synthetic-broker.log")
                build_logs = sorted(
                    state_home.rglob("build-*.log"), key=lambda path: path.stat().st_mtime
                )
                if build_logs:
                    shutil.copyfile(build_logs[-1], failure_dir / "development-build.log")
                backup_stopped_journal(state_home, environment_name, failure_dir)
                print(f"Sanitized synthetic failure evidence: {failure_dir}", file=sys.stderr)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"Solana rehearsal failed: {error}", file=sys.stderr)
        raise SystemExit(1)
