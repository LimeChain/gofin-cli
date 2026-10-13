"""Named-plan rehearsal for a limit create, update, restart and cancellation."""
from __future__ import annotations

import json
from pathlib import Path
import sqlite3
import subprocess
import sys

from solana_perps_tpsl_plan import (
    _call,
    _find_binding_value,
    _network_fee,
    _restart,
    _summary,
    _token_amount,
)

REPO = Path(__file__).resolve().parent.parent
DEV = REPO / "scripts/fin-dev.py"
USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"


def _plan(wallet_address: str, position: str) -> dict:
    resource = f"solana:mainnet-beta:{wallet_address}"
    native = {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}
    usdc = {"network": "solana:mainnet-beta", "reference": USDC_MINT, "decimals": 6}
    margin = {"network": "solana:mainnet-beta", "reference": "jupiter:margin-usd", "decimals": 6}
    network = {"network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal"}
    create_input = {
        **network,
        "market": "SOL",
        "side": "long",
        "funding_token": "USDC",
        "collateral_amount_atoms": "100000",
        "size_usd_atoms": "110000",
        "trigger_price_usd_atoms": "150000000",
        "trigger_above_threshold": True,
        "maximum_protocol_fee_atoms": "100000",
        "maximum_protocol_fee_usd_atoms": "100000",
        "maximum_network_fee_lamports": "15000",
        "maximum_account_funding_lamports": "7273200",
    }
    create_scope = {
        "maximum_size_usd_atoms": "110000",
        "maximum_collateral_atoms": "100000",
        "maximum_protocol_fee_atoms": "100000",
        "maximum_protocol_fee_usd_atoms": "100000",
        "maximum_slippage_bps": 50,
        "maximum_network_fee_lamports": 15000,
        "maximum_account_funding_lamports": 7273200,
    }
    update_input = {
        **network,
        "request": None,
        "original_operation_id": None,
        "original_submission": None,
        "trigger_price_usd_atoms": "160000000",
        "maximum_network_fee_lamports": "15000",
    }
    update_scope = {
        "minimum_trigger_price_usd_atoms": "160000000",
        "maximum_trigger_price_usd_atoms": "160000000",
        "maximum_network_fee_lamports": 15000,
        "recovery_parent_step_id": "create",
    }
    cancel_input = {
        **network,
        "request": None,
        "original_operation_id": None,
        "original_submission": None,
        "maximum_network_fee_lamports": "5000",
    }
    cancel_scope = {
        "maximum_network_fee_lamports": 5000,
        "recovery_parent_step_id": "create",
    }
    return {
        "version": 1,
        "name": "Synthetic Perps limit create, update and recovery cancellation",
        "steps": [
            {"step_id": "create", "operation": "jupiter.perps.limit.create", "input": create_input,
             "permitted_variations": create_scope, "completion": {"operation_succeeded": True},
             "funding_dependencies": []},
            {"step_id": "update", "operation": "jupiter.perps.limit.update", "input": update_input,
             "permitted_variations": update_scope, "completion": {"operation_succeeded": True},
             "funding_dependencies": []},
            {"step_id": "cancel", "operation": "jupiter.perps.limit.cancel", "input": cancel_input,
             "permitted_variations": cancel_scope, "completion": {"operation_succeeded": True},
             "funding_dependencies": []},
        ],
        "aggregate_bounds": {
            "maximum_debits": [
                {"resource": resource, "amount": {"asset": usdc, "atoms": "100000"}},
                {"resource": resource, "amount": {"asset": native, "atoms": "7273200"}},
            ],
            "maximum_costs": [
                {"location": {"kind": "cash", "resource": resource},
                 "amount": {"asset": native, "atoms": "35000"}, "treatment": "additional_debit"},
                {"location": {"kind": "position", "resource": resource,
                               "position": f"{position}:collateral", "position_kind": "locked_collateral"},
                 "amount": {"asset": margin, "atoms": "100000"}, "treatment": "included_in_debit"},
            ],
        },
    }


def _reservation_count(env: dict[str, str], environment: str) -> int:
    state_home = Path(env["FIN_DEV_STATE_HOME"])
    for metadata_path in state_home.rglob("environment.json"):
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if metadata.get("name") != environment:
                continue
            database = Path(metadata["database"]).resolve(strict=True)
            with sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True, timeout=10) as db:
                return int(db.execute("SELECT COUNT(*) FROM reservations").fetchone()[0])
        except (OSError, KeyError, ValueError, TypeError, sqlite3.Error):
            continue
    raise AssertionError("Could not inspect the synthetic limit-plan journal reservations.")


def run_named_plan_limit(
    environment: str, env: dict[str, str], initial_usdc_atoms: int, initial_lamports: int,
) -> dict[str, object]:
    """Run the public CLI plan through placement, amendment, restart and cancel."""
    network = {"network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal"}
    balances = _call(environment, ["read", "--input", json.dumps(network), "wallet.balances", "--json"], env)
    wallet_address = balances.get("wallet")
    if not isinstance(wallet_address, str):
        raise AssertionError("Public wallet read omitted the selected wallet address.")
    draft = _plan(wallet_address, "preview-position-placeholder")
    create_input = draft["steps"][0]["input"]
    candidate = _call(environment, [
        "preview", "jupiter.perps.limit.create", "--input", json.dumps(create_input), "--json",
    ], env)
    position = _find_binding_value(candidate, "position")
    if not position:
        raise AssertionError("Read-only limit preview omitted the deterministic position PDA.")
    plan = _plan(wallet_address, position)
    payload = json.dumps(plan, separators=(",", ":"))
    preview = _call(environment, ["plans", "preview", "--input", payload, "--json"], env)
    review_hash = preview.get("review_hash")
    if not isinstance(review_hash, str) or not review_hash:
        raise AssertionError(f"Limit plan preview omitted its review hash: {json.dumps(preview, sort_keys=True)}")
    approved = _call(environment, [
        "plans", "authorize", "--input", payload, "--id", "solana-perps-limit-plan",
        "--valid-for", "10m", "--review-hash", review_hash, "--json",
    ], env)
    plan_id = approved.get("plan_id") or approved.get("id")
    if not isinstance(plan_id, str) or not plan_id:
        raise AssertionError("Limit plan authorization omitted its durable plan ID.")

    records: dict[str, dict] = {}
    created = _call(environment, [
        "execute", "jupiter.perps.limit.create", "--input", json.dumps(plan["steps"][0]["input"]),
        "--id", "solana-limit-plan-create", "--plan", plan_id, "--step", "create", "--json",
    ], env)
    records["create"] = created
    if created.get("status") != "pending" or not (created.get("effects") or {}).get("proof_digest"):
        raise AssertionError(f"Limit create did not prove open escrow while pending: {json.dumps(created, sort_keys=True)}")
    request_id = created.get("id")
    binding = (created.get("preview") or {}).get("binding") or {}
    request = binding.get("request")
    if not isinstance(request_id, str) or not isinstance(request, str):
        raise AssertionError("Limit create omitted its durable operation or request identity.")

    updated = _call(environment, [
        "execute", "jupiter.perps.limit.update", "--input", json.dumps(plan["steps"][1]["input"]),
        "--id", "solana-limit-plan-update", "--plan", plan_id, "--step", "update", "--json",
    ], env)
    records["update"] = updated
    if updated.get("status") != "completed" or not (updated.get("effects") or {}).get("proof_digest"):
        raise AssertionError(f"Linked limit update did not settle: {json.dumps(updated, sort_keys=True)}")

    transaction_count = len(json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", []))
    _restart(environment, env)
    after_restart = _call(environment, ["status", request_id, "--json"], env)
    if after_restart.get("status") != "pending" or after_restart.get("error") is not None:
        raise AssertionError(f"Original limit create did not remain pending after restart: {json.dumps(after_restart, sort_keys=True)}")
    if len(json.loads(Path(env["FIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", [])) != transaction_count:
        raise AssertionError("Restart reconciliation unexpectedly broadcast or duplicated a transaction.")

    cancelled = _call(environment, [
        "execute", "jupiter.perps.limit.cancel", "--input", json.dumps(plan["steps"][2]["input"]),
        "--id", "solana-limit-plan-cancel", "--plan", plan_id, "--step", "cancel", "--json",
    ], env)
    records["cancel"] = cancelled
    if cancelled.get("status") != "completed" or not (cancelled.get("effects") or {}).get("proof_digest"):
        raise AssertionError(f"Recovery cancellation did not settle from exact close proof: {json.dumps(cancelled, sort_keys=True)}")
    parent = _call(environment, ["status", request_id, "--json"], env)
    records["parent_refund"] = parent
    if parent.get("status") != "completed" or not (parent.get("effects") or {}).get("proof_digest"):
        raise AssertionError(f"Create parent did not settle its exact refund: {json.dumps(parent, sort_keys=True)}")

    final_balances = _call(environment, ["read", "--input", json.dumps(network), "wallet.balances", "--json"], env)
    actual_usdc = _token_amount(final_balances, USDC_MINT)
    actual_lamports = int((final_balances.get("native") or {}).get("amount_atoms", -1))
    fees = sum(_network_fee(records[name]) for name in ("create", "update", "cancel"))
    funding_rows = [
        effect for effect in ((parent.get("effects") or {}).get("effects") or [])
        if effect.get("kind") == "account_funding_progress"
    ]
    retained_funding = sum(int(effect.get("remaining", "0")) for effect in funding_rows)
    position = binding.get("position")
    position_rent_rows = [
        effect for effect in funding_rows
        if effect.get("account") == position and effect.get("state") == "open"
    ]
    temporary_rows = [effect for effect in funding_rows if effect.get("account") != position]
    if (
        len(position_rent_rows) != 1
        or retained_funding != int(position_rent_rows[0]["remaining"])
        or any(effect.get("state") != "closed" or effect.get("remaining") != "0" for effect in temporary_rows)
    ):
        raise AssertionError(
            "The finalized cancellation did not close temporary request funding while retaining only the verified Position PDA rent."
        )
    expected_lamports = initial_lamports - fees - retained_funding
    if actual_usdc != initial_usdc_atoms or actual_lamports != expected_lamports:
        raise AssertionError(
            f"Limit plan restoration mismatch: USDC {actual_usdc}/{initial_usdc_atoms}; "
            f"lamports {actual_lamports}, expected {expected_lamports} including {retained_funding} lamports of verified position rent."
        )
    plan_status = _call(environment, ["plans", "status", plan_id, "--json"], env)
    if plan_status.get("next_eligible_step_id") is not None:
        raise AssertionError(f"Limit plan did not finish after parent refund: {json.dumps(plan_status, sort_keys=True)}")
    reservations = _reservation_count(env, environment)
    if reservations != 0:
        raise AssertionError("Limit plan terminal path left reservations behind.")
    return {
        "plan_id": plan_id,
        "request": request,
        "steps": {name: _summary(record) for name, record in records.items()},
        "plan_status": plan_status,
        "reservation_count": reservations,
        "restoration": {
            "expected_usdc_atoms": str(initial_usdc_atoms),
            "observed_usdc_atoms": str(actual_usdc),
            "initial_lamports": str(initial_lamports),
            "network_fee_lamports": str(fees),
            "retained_position_rent_lamports": str(retained_funding),
            "observed_lamports": str(actual_lamports),
        },
    }
