#!/usr/bin/env python3
"""Prepare unsigned Hyperliquid acceptance-plan artifacts using Fin reads/previews only.

This utility never starts Fin, enrolls credentials, authorizes a plan, or submits a
financial action. It is intentionally fail-closed when fresh Fin evidence cannot
support a fixed-size plan.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
import uuid
from decimal import Decimal, InvalidOperation, ROUND_CEILING, ROUND_FLOOR, getcontext
from pathlib import Path
from typing import Any

getcontext().prec = 100


ADDRESS = re.compile(r"^0x[0-9a-fA-F]{40}$")
USDC_ARB = {"network": "ethereum:42161", "reference": "0xaf88d065e77c8cc2239327c5edb3a432268e5831", "decimals": 6}
USDC_HL = {"network": "hyperliquid:mainnet", "reference": "USDC", "decimals": 36}
ALLOWED_READS = {
    "hyperliquid.markets", "hyperliquid.order_book", "hyperliquid.balances",
    "hyperliquid.positions", "hyperliquid.open_orders", "hyperliquid.trading_state",
    "hyperliquid.abstraction", "hyperliquid.fees", "wallet.balances",
}
UNRESOLVED = {"pending", "submitted", "unknown", "outcome_unknown", "awaiting_reconciliation"}


class PlanError(Exception):
    pass


def compact(value: Any) -> str:
    return json.dumps(value, separators=(",", ":"), sort_keys=True)


def exact_decimal(value: Any, label: str, *, positive: bool = False) -> Decimal:
    if not isinstance(value, (str, int)) or isinstance(value, bool):
        raise PlanError(f"{label} is not an exact decimal string")
    try:
        result = Decimal(str(value))
    except InvalidOperation as exc:
        raise PlanError(f"{label} is not a valid decimal") from exc
    if not result.is_finite() or (positive and result <= 0):
        raise PlanError(f"{label} must be finite" + (" and positive" if positive else ""))
    return result


def fmt(value: Decimal) -> str:
    return format(value.normalize(), "f") if value else "0"


def invoke(fin: str, argv: list[str], allowed: set[str]) -> Any:
    """Invoke a closed read/preview/operations allowlist, without a shell."""
    ok = False
    if argv and argv[0] == "read" and len(argv) > 1 and argv[1] in ALLOWED_READS and argv[1] in allowed:
        ok = True
    elif argv[:1] == ["operations"] and "operations" in allowed:
        ok = True
    elif argv[:1] == ["snapshot"] and "snapshot" in allowed:
        ok = True
    elif argv[:2] == ["plans", "preview"] and "plans" in allowed:
        ok = True
    if not ok:
        raise PlanError("internal refusal: command is outside the read/preview-only allowlist")
    try:
        response = subprocess.run([fin, *argv], check=False, capture_output=True, text=True, timeout=180)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise PlanError(f"Fin read-only command did not complete: {exc}") from exc
    if response.returncode:
        detail = response.stderr.strip() or response.stdout.strip() or "no diagnostic"
        raise PlanError(f"Fin read-only command failed ({response.returncode}): {detail}")
    try:
        value = json.loads(response.stdout)
    except json.JSONDecodeError as exc:
        raise PlanError("Fin did not return valid JSON") from exc
    if argv[:1] == ["operations"] and isinstance(value, list):
        return value
    if not isinstance(value, dict):
        raise PlanError("Fin returned an unexpected JSON shape")
    if value.get("status") in {"error", "failed"} or value.get("error"):
        raise PlanError(f"Fin returned an error: {value.get('error', value.get('message', 'read failed'))}")
    return value


def payload(result: dict[str, Any]) -> Any:
    return result.get("data", result)


def read(fin: str, operation: str, input_value: dict[str, Any]) -> dict[str, Any]:
    return invoke(fin, ["read", operation, "--input", compact(input_value), "--json"], {operation})


def wallet_alias_matches(result: dict[str, Any], wallet_address: str) -> bool:
    # `wallet.balances` exposes the resolved account at the top level while
    # its `data` is a list of ResourceAmount rows.
    resource = result.get("resource") if isinstance(result, dict) else None
    expected = f"ethereum:42161:{wallet_address.lower()}"
    return isinstance(resource, str) and resource.lower() == expected


def check_no_pending_operations(result: dict[str, Any]) -> None:
    rows = payload(result) if isinstance(result, dict) else result
    if isinstance(rows, dict):
        rows = rows.get("operations")
    if not isinstance(rows, list):
        raise PlanError("Fin operations did not return a list; cannot verify there are no unresolved writes")
    found = []
    for row in rows:
        if not isinstance(row, dict):
            raise PlanError("Fin operations contains an invalid row")
        status = str(row.get("status", "")).lower().replace("-", "_")
        if status in UNRESOLVED:
            found.append(row.get("id", row.get("operation", "unidentified")))
    if found:
        raise PlanError("Unresolved Fin operation(s) exist; reconcile them before preparing this plan")


def unwrap_array(result: dict[str, Any], label: str) -> list[Any]:
    value = payload(result)
    if isinstance(value, list):
        return value
    if isinstance(value, dict):
        for key in ("rows", "orders", "positions", "markets", "data"):
            if isinstance(value.get(key), list):
                return value[key]
    raise PlanError(f"{label} returned an unexpected array shape")


def round_step(value: Decimal, decimals: int, rounding: str) -> Decimal:
    quantum = Decimal(1).scaleb(-decimals)
    return value.quantize(quantum, rounding=rounding)


def round_price(value: Decimal, sz_decimals: int, *, price_decimals: int, upward: bool) -> Decimal:
    """Round to a conservative Hyperliquid five-significant-digit price step."""
    if value <= 0:
        raise PlanError("market price must be positive")
    exponent = value.adjusted() - 4
    sig_step = Decimal(1).scaleb(exponent)
    decimal_step = Decimal(1).scaleb(-(price_decimals - sz_decimals))
    step = max(sig_step, decimal_step)
    units = (value / step).to_integral_value(rounding=ROUND_CEILING if upward else ROUND_FLOOR)
    result = units * step
    if result <= 0:
        raise PlanError("native price rounding produced a nonpositive price")
    return result


def floor_size(value: Decimal, decimals: int) -> Decimal:
    if not 0 <= decimals <= 18:
        raise PlanError("market size precision is invalid")
    return round_step(value, decimals, ROUND_FLOOR)


def best_prices(book: Any, native_coin: str) -> tuple[Decimal, Decimal]:
    if not isinstance(book, dict) or book.get("coin") != native_coin:
        raise PlanError("order book does not match exact native market identity")
    levels = book.get("levels")
    if not isinstance(levels, list) or len(levels) != 2 or not all(isinstance(x, list) and x for x in levels):
        raise PlanError("fresh order book is incomplete")
    bid = exact_decimal(levels[0][0].get("px"), "best bid", positive=True)
    ask = exact_decimal(levels[1][0].get("px"), "best ask", positive=True)
    if bid > ask:
        raise PlanError("order book is crossed; refuse to derive bounded prices")
    return bid, ask


def market_by_symbol(rows: list[Any], symbol: str, kind: str) -> dict[str, Any]:
    matches = [row for row in rows if isinstance(row, dict) and row.get("symbol") == symbol and row.get("kind") == kind]
    if len(matches) != 1 or matches[0].get("is_delisted"):
        raise PlanError(f"fresh metadata does not identify one active {kind} market for {symbol}")
    row = matches[0]
    if (not isinstance(row.get("native_coin"), str) or not row["native_coin"]
            or not isinstance(row.get("sz_decimals"), int) or isinstance(row.get("sz_decimals"), bool)
            or not isinstance(row.get("base_asset"), dict)
            or not isinstance(row["base_asset"].get("reference"), str)):
        raise PlanError(f"fresh metadata for {symbol} omitted exact native/base identity or size precision")
    return row


def balances_by_coin(raw: Any) -> dict[str, tuple[Decimal, Decimal]]:
    if not isinstance(raw, dict) or not isinstance(raw.get("balances"), list):
        raise PlanError("spot balances are missing exact native rows")
    output = {}
    for row in raw["balances"]:
        if not isinstance(row, dict) or not isinstance(row.get("coin"), str):
            raise PlanError("spot balance row has no coin identity")
        total = exact_decimal(row.get("total"), f"{row['coin']} total")
        hold = exact_decimal(row.get("hold"), f"{row['coin']} hold")
        if total < 0 or hold < 0 or hold > total:
            raise PlanError(f"{row['coin']} balance/hold is inconsistent")
        if row["coin"] in output:
            raise PlanError(f"duplicate {row['coin']} balance rows make the available amount ambiguous")
        output[row["coin"]] = (total, hold)
    return output


def nonzero_eth_positions(raw: Any) -> bool:
    rows = raw.get("assetPositions") if isinstance(raw, dict) else None
    if not isinstance(rows, list):
        raise PlanError("perpetual positions are missing")
    for row in rows:
        position = row.get("position", {}) if isinstance(row, dict) else {}
        if position.get("coin") == "ETH" and exact_decimal(position.get("szi", "0"), "ETH position") != 0:
            return True
    return False


def active_orders(raw: Any) -> list[Any]:
    if not isinstance(raw, list):
        raise PlanError("open orders did not return a native array")
    return raw


def atoms(value: Decimal) -> str:
    scaled = value * (Decimal(10) ** 36)
    if scaled != scaled.to_integral_value():
        raise PlanError(f"quantity {fmt(value)} cannot be represented exactly at Fin's 36-decimal precision")
    return str(int(scaled))


def atoms_ceil(value: Decimal) -> str:
    if value < 0:
        raise PlanError("financial quantity cannot be negative")
    scaled = value * (Decimal(10) ** 36)
    return str(int(scaled.to_integral_value(rounding=ROUND_CEILING)))


def atoms_floor(value: Decimal) -> str:
    if value < 0:
        raise PlanError("minimum credit cannot be negative")
    scaled = value * (Decimal(10) ** 36)
    return str(int(scaled.to_integral_value(rounding=ROUND_FLOOR)))


def amount(asset: dict[str, Any], value: Decimal) -> dict[str, Any]:
    return {"asset": asset, "atoms": atoms(value)}


def cost(resource: str, asset: dict[str, Any], value: Decimal, treatment: str) -> dict[str, Any]:
    return {"location": {"kind": "cash", "resource": resource}, "amount": {"asset": asset, "atoms": atoms_ceil(value)},
            "treatment": treatment}


def exact_scope(debits: list[dict[str, Any]], costs: list[dict[str, Any]], effects: list[dict[str, Any]]) -> dict[str, Any]:
    credits = []
    for effect in effects:
        kind = effect.get("kind")
        if kind == "conversion":
            identity = {"resource": effect["resource"], "asset": effect["output"]}
        elif kind == "transfer":
            identity = {"resource": effect["to"], "asset": effect["credit_asset"]}
        else:
            continue
        if identity not in credits:
            credits.append(identity)
    return {"mode": "exact", "maximum_debits": debits, "maximum_costs": costs,
            "possible_credits": credits}


def aggregate_amounts(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    totals: dict[tuple[str, str], tuple[dict[str, Any], int]] = {}
    for entry in entries:
        resource, amount_value = entry.get("resource"), entry.get("amount")
        if not isinstance(resource, str) or not isinstance(amount_value, dict):
            raise PlanError("plan debit omitted an exact resource or amount")
        asset_value, atoms_value = amount_value.get("asset"), amount_value.get("atoms")
        if not isinstance(asset_value, dict) or not isinstance(atoms_value, str) or not atoms_value.isdecimal():
            raise PlanError("plan debit omitted exact asset atoms")
        key = (resource, compact(asset_value))
        if key not in totals:
            totals[key] = ({"resource": resource, "amount": {"asset": asset_value, "atoms": "0"}}, 0)
        template, old = totals[key]
        totals[key] = (template, old + int(atoms_value))
    result = []
    for template, total in totals.values():
        template["amount"]["atoms"] = str(total)
        result.append(template)
    return result


def aggregate_costs(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    totals: dict[tuple[str, str, str], tuple[dict[str, Any], int]] = {}
    for entry in entries:
        location, amount_value, treatment = entry.get("location"), entry.get("amount"), entry.get("treatment")
        if not isinstance(location, dict) or not isinstance(amount_value, dict) or not isinstance(treatment, str):
            raise PlanError("plan cost bound is incomplete")
        asset_value, atoms_value = amount_value.get("asset"), amount_value.get("atoms")
        if not isinstance(asset_value, dict) or not isinstance(atoms_value, str) or not atoms_value.isdecimal():
            raise PlanError("plan cost bound omitted exact asset atoms")
        key = (compact(location), compact(asset_value), treatment)
        if key not in totals:
            totals[key] = (json.loads(json.dumps(entry)), 0)
        template, old = totals[key]
        totals[key] = (template, old + int(atoms_value))
    result = []
    for template, total in totals.values():
        template["amount"]["atoms"] = str(total)
        result.append(template)
    return result


def step(step_id: str, operation: str, input_value: dict[str, Any], scope: dict[str, Any],
         completion: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "step_id": step_id,
        "operation": operation,
        "input": input_value,
        "permitted_variations": scope,
        "completion": completion or {"operation_succeeded": True},
        "funding_dependencies": [],
    }


def gather(fin: str, account: str, wallet: str, wallet_address: str) -> dict[str, Any]:
    operations = invoke(fin, ["operations", "--json"], {"operations"})
    check_no_pending_operations(operations)
    common = {"account": account}
    snapshot = invoke(fin, ["snapshot", "--json"], {"snapshot"})
    state = snapshot.get("state") if isinstance(snapshot, dict) else None
    authorities = state.get("authorities") if isinstance(state, dict) else None
    owner_prefix = f"hyperliquid:mainnet:{wallet_address.lower()}:"
    if not isinstance(authorities, list) or not any(
        row.get("authority") == "hyperliquid:native:account_role"
        and row.get("value") == "master_user"
        and str(row.get("resource", "")).startswith(owner_prefix)
        for row in authorities if isinstance(row, dict)
    ):
        raise PlanError("Fin snapshot did not verify this wallet as the enrolled Hyperliquid master owner")
    result = {
        "operations": operations,
        "snapshot": snapshot,
        "spot_markets": read(fin, "hyperliquid.markets", {"kind": "spot"}),
        "perp_markets": read(fin, "hyperliquid.markets", {"kind": "perpetual"}),
        "spot_book": read(fin, "hyperliquid.order_book", {"symbol": "HYPE/USDC"}),
        "eth_book": read(fin, "hyperliquid.order_book", {"symbol": "ETH/USDC:USDC"}),
        "balances": read(fin, "hyperliquid.balances", common),
        "positions": read(fin, "hyperliquid.positions", common),
        "open_orders": read(fin, "hyperliquid.open_orders", common),
        "trading_state": read(fin, "hyperliquid.trading_state", {**common, "symbol": "ETH/USDC:USDC"}),
        "abstraction": read(fin, "hyperliquid.abstraction", common),
        "fees": read(fin, "hyperliquid.fees", common),
        "wallet_balances": read(fin, "wallet.balances", {"wallet": wallet, "network": "evm", "chain_id": 42161, "tokens": [USDC_ARB["reference"]]}),
    }
    # Caller address must be the exact address Fin resolved for this wallet alias.
    if not wallet_alias_matches(result["wallet_balances"], wallet_address):
        raise PlanError("reviewed Arbitrum address does not match the selected Fin wallet alias")
    return result


def build_plan(evidence: dict[str, Any], account: str, wallet_address: str,
               max_fee_usdc: Decimal, max_loss_usdc: Decimal) -> tuple[dict[str, Any], dict[str, Any]]:
    spot_markets = unwrap_array(evidence["spot_markets"], "spot markets")
    perp_markets = unwrap_array(evidence["perp_markets"], "perpetual markets")
    hype = market_by_symbol(spot_markets, "HYPE/USDC", "spot")
    eth = market_by_symbol(perp_markets, "ETH/USDC:USDC", "perpetual")
    if hype.get("quote_asset") != {"reference": "0x6d1e7cde53ba9467b783cb7c530ce054", "symbol": "USDC", "decimals": 8, "token_index": 0} and not (
        isinstance(hype.get("quote_asset"), dict)
        and hype["quote_asset"].get("symbol") == "USDC"
        and hype["quote_asset"].get("token_index") == 0
        and hype["quote_asset"].get("decimals") == 8
        and hype["quote_asset"].get("reference", "").lower() == "0x6d1e7cde53ba9467b783cb7c530ce054"
    ):
        raise PlanError("HYPE market quote is not the verified native USDC spot token")
    eth_quote = eth.get("quote_asset")
    if (not isinstance(eth_quote, dict) or eth_quote.get("symbol") != "USDC"
            or eth_quote.get("reference") != "USDC" or eth_quote.get("decimals") != 6
            or eth_quote.get("token_index") is not None):
        raise PlanError("ETH perpetual quote is not the verified primary USDC collateral asset")
    spot_balance = payload(evidence["balances"])
    if not isinstance(spot_balance, dict) or not isinstance(spot_balance.get("owner"), str):
        raise PlanError("Hyperliquid balances omitted the protected owner identity")
    if spot_balance["owner"].lower() != wallet_address.lower():
        raise PlanError("Hyperliquid signer owner and reviewed Arbitrum wallet do not match")
    abstraction = payload(evidence["abstraction"])
    native_mode = abstraction.get("base") if isinstance(abstraction, dict) else None
    mode = {"default": "separate", "disabled": "separate", "unifiedAccount": "unified"}.get(native_mode, native_mode)
    if mode not in ("separate", "unified"):
        raise PlanError("This plan requires verified standard separate or unified cash semantics")
    if abstraction.get("dex") not in (False, None):
        raise PlanError("DEX abstraction is not supported by this bounded test plan")
    if nonzero_eth_positions(payload(evidence["positions"])):
        raise PlanError("ETH already has exposure; this plan will not alter or assume restoration of it")
    if active_orders(payload(evidence["open_orders"])):
        raise PlanError("Open Hyperliquid orders exist; cancel/reconcile them before planning")

    bid_h, ask_h = best_prices(payload(evidence["spot_book"]), hype["native_coin"])
    bid_e, ask_e = best_prices(payload(evidence["eth_book"]), eth["native_coin"])
    hype_sz_decimals = int(hype["sz_decimals"])
    eth_sz_decimals = int(eth["sz_decimals"])
    buy_min = round_price(bid_h * Decimal("0.995"), hype_sz_decimals, price_decimals=8, upward=True)
    buy_max = round_price(ask_h * Decimal("1.005"), hype_sz_decimals, price_decimals=8, upward=False)
    sell_min = round_price(bid_h * Decimal("0.995"), hype_sz_decimals, price_decimals=8, upward=True)
    sell_max = round_price(bid_h * Decimal("1.005"), hype_sz_decimals, price_decimals=8, upward=False)
    eth_buy_min = round_price(bid_e * Decimal("0.995"), eth_sz_decimals, price_decimals=6, upward=True)
    eth_buy_max = round_price(ask_e * Decimal("1.005"), eth_sz_decimals, price_decimals=6, upward=False)
    eth_sell_min = round_price(bid_e * Decimal("0.995"), eth_sz_decimals, price_decimals=6, upward=True)
    eth_sell_max = round_price(bid_e * Decimal("1.005"), eth_sz_decimals, price_decimals=6, upward=False)
    if buy_min > buy_max or sell_min > sell_max or eth_buy_min > eth_buy_max or eth_sell_min > eth_sell_max:
        raise PlanError("current price ranges cannot be represented at native precision")
    hype_size = floor_size(Decimal("11") / buy_max, hype_sz_decimals)
    eth_size = floor_size(Decimal("11") / eth_buy_max, eth_sz_decimals)
    if hype_size <= 0 or eth_size <= 0:
        raise PlanError("market precision rounds the $11 target to zero")
    if hype_size * buy_max > 12 or eth_size * eth_buy_max > 12:
        raise PlanError("rounded order size exceeds the $12 notional ceiling")
    spot_roundtrip_loss = max(Decimal(0), hype_size * (buy_max - sell_min))
    total_risk_cap = (max_fee_usdc * 4) + (max_loss_usdc * 2) + Decimal("1") + spot_roundtrip_loss
    if total_risk_cap > Decimal("5"):
        raise PlanError("cumulative fees, spot roundtrip price loss, realized-loss caps and withdrawal fee exceed the $5 plan risk ceiling")
    if hype_size * buy_min < 10 or hype_size * sell_min < 10 or eth_size * eth_buy_min < 10:
        raise PlanError("native minimum notional would not be met at the approved lower price bound")
    balances = balances_by_coin(payload(spot_balance).get("spot"))
    hype_total, hype_hold = balances.get("HYPE", (Decimal(0), Decimal(0)))
    hype_free = hype_total - hype_hold
    required_fee_dust = (max_fee_usdc / buy_min) + (max_fee_usdc / sell_min)
    if hype_free < required_fee_dust:
        raise PlanError("existing free HYPE dust is insufficient to cover worst-case base-denominated buy/sell fees")
    usdc_total, usdc_hold = balances.get("USDC", (Decimal(0), Decimal(0)))
    spot_free_usdc = usdc_total - usdc_hold
    perpetual = spot_balance.get("perpetual")
    perp_free = exact_decimal(perpetual.get("withdrawable"), "perpetual withdrawable USDC") if isinstance(perpetual, dict) else Decimal(0)
    if mode == "unified":
        cash_resource = f"hyperliquid:mainnet:{wallet_address.lower()}:unified:primary"
        available_cash = spot_free_usdc
        if available_cash < Decimal("12") + Decimal("5") + (max_fee_usdc * 4) + (max_loss_usdc * 2):
            raise PlanError("verified unified free USDC cannot cover the plan's worst-case buy, fees, losses and withdrawal")
    else:
        cash_resource = f"hyperliquid:mainnet:{wallet_address.lower()}:perpetual:primary"
        if spot_free_usdc < Decimal("12") + max_fee_usdc:
            raise PlanError("separate spot USDC cannot cover the HYPE buy; prepare and review a separate exact class transfer first")
        if perp_free < Decimal("5") + (max_loss_usdc * 2) + (max_fee_usdc * 2):
            raise PlanError("separate perpetual USDC cannot cover ETH loss/fee bounds and the withdrawal")

    trading = payload(evidence["trading_state"])
    state = trading.get("state") if isinstance(trading, dict) else None
    if not isinstance(state, dict):
        raise PlanError("ETH trading state is missing")
    native_leverage = state.get("leverage")
    leverage = native_leverage.get("value") if isinstance(native_leverage, dict) else None
    margin_type = native_leverage.get("type") if isinstance(native_leverage, dict) else None
    cross = margin_type == "cross"
    if not isinstance(leverage, int) or isinstance(leverage, bool) or leverage < 1 or margin_type not in {"cross", "isolated"}:
        raise PlanError("ETH trading state lacks exact original leverage/margin mode")

    spot_resource = cash_resource if mode == "unified" else f"hyperliquid:mainnet:{wallet_address.lower()}:spot:primary"
    eth_resource = cash_resource
    derivative_resource = f"hyperliquid:mainnet:{wallet_address.lower()}:perpetual:primary"
    base_asset = {"network": "hyperliquid:mainnet", "reference": hype["base_asset"]["reference"], "decimals": 36}
    eth_asset = {"network": "hyperliquid:mainnet", "reference": eth["base_asset"]["reference"], "decimals": 36}

    specs: list[tuple[str, str, dict[str, Any], dict[str, Any]]] = []
    def request_id(label: str) -> str:
        return f"hl-human-{label}-{uuid.uuid4()}"
    hype_buy_costs = [
        cost(spot_resource, base_asset, max_fee_usdc / buy_min, "deducted_from_credit"),
        cost(spot_resource, USDC_HL, max_fee_usdc, "additional_debit"),
    ]
    hype_buy_debits = [{"resource": spot_resource, "amount": amount(USDC_HL, Decimal("12"))}]
    hype_buy_effects = [{"kind": "conversion", "resource": spot_resource, "input": USDC_HL,
                         "maximum_input": atoms(Decimal("12")), "output": base_asset, "minimum_output": atoms(Decimal(0))}]
    specs.append(("hyperliquid.orders.place", "hype-limit-buy", {
        "request_id": request_id("hype-buy"), "account": account, "symbol": "HYPE/USDC", "market_type": "spot",
        "side": "buy", "order_type": "limit", "size": fmt(hype_size), "limit_price": fmt(buy_max),
        "min_price": fmt(buy_min), "max_price": fmt(buy_max), "time_in_force": "ioc",
        "max_notional_usdc": "12", "max_fee_usdc": fmt(max_fee_usdc), "reduce_only": False,
    }, exact_scope(hype_buy_debits, hype_buy_costs, hype_buy_effects)))
    hype_sell_costs = [
        cost(spot_resource, base_asset, max_fee_usdc / sell_min, "additional_debit"),
        cost(spot_resource, USDC_HL, max_fee_usdc, "deducted_from_credit"),
    ]
    hype_sell_debits = [{"resource": spot_resource, "amount": amount(base_asset, hype_size)}]
    hype_sell_effects = [{"kind": "conversion", "resource": spot_resource, "input": base_asset,
                         "maximum_input": atoms(hype_size), "output": USDC_HL, "minimum_output": atoms(Decimal(0))}]
    specs.append(("hyperliquid.orders.place", "hype-bounded-market-sell", {
        "request_id": request_id("hype-sell"), "account": account, "symbol": "HYPE/USDC", "market_type": "spot",
        "side": "sell", "order_type": "market", "size": fmt(hype_size), "min_price": fmt(sell_min),
        "max_price": fmt(sell_max),
        "time_in_force": "ioc", "max_notional_usdc": "12", "max_fee_usdc": fmt(max_fee_usdc), "reduce_only": False,
    }, exact_scope(hype_sell_debits, hype_sell_costs, hype_sell_effects)))
    specs.append(("hyperliquid.account.set_leverage", "set-eth-isolated-1x", {
        "request_id": request_id("isolated"), "account": account, "symbol": "ETH/USDC:USDC", "leverage": 1, "cross": False,
    }, exact_scope([], [], [])))
    perp_costs = [cost(eth_resource, USDC_HL, max_fee_usdc, "additional_debit")]
    perp_debits = []
    if max_loss_usdc > 0:
        perp_debits.append({"resource": eth_resource, "amount": amount(USDC_HL, max_loss_usdc)})
        perp_costs.append(cost(eth_resource, USDC_HL, max_loss_usdc, "included_in_debit"))
    perp_effects = [
        {"kind": "derivative_position", "resource": derivative_resource,
         "position": "ETH/USDC:USDC:long", "asset": eth_asset,
         "position_kind": "derivative_long", "minimum_delta": atoms(-eth_size), "maximum_delta": atoms(eth_size)},
        {"kind": "derivative_position", "resource": derivative_resource,
         "position": "ETH/USDC:USDC:short", "asset": eth_asset,
         "position_kind": "derivative_short", "minimum_delta": atoms(-eth_size), "maximum_delta": atoms(eth_size)},
        {"kind": "settlement", "resource": eth_resource, "asset": USDC_HL,
         "category": "realized_pnl", "minimum_delta": "0", "maximum_delta": atoms(Decimal("12"))},
    ]
    if max_loss_usdc > 0:
        perp_effects.append({"kind": "settlement", "resource": eth_resource, "asset": USDC_HL,
                             "category": "realized_pnl", "minimum_delta": "-" + atoms(max_loss_usdc), "maximum_delta": "0"})
    perp_scope = exact_scope(perp_debits, perp_costs, perp_effects)
    specs.append(("hyperliquid.orders.place", "eth-perp-open", {
        "request_id": request_id("eth-open"), "account": account, "symbol": "ETH/USDC:USDC", "market_type": "perpetual",
        "side": "buy", "order_type": "limit", "size": fmt(eth_size), "limit_price": fmt(eth_buy_max),
        "min_price": fmt(eth_buy_min), "max_price": fmt(eth_buy_max), "time_in_force": "ioc",
        "max_notional_usdc": "12", "max_fee_usdc": fmt(max_fee_usdc), "max_realized_loss_usdc": fmt(max_loss_usdc), "reduce_only": False,
    }, perp_scope))
    specs.append(("hyperliquid.orders.place", "eth-perp-reduce-only-close", {
        "request_id": request_id("eth-close"), "account": account, "symbol": "ETH/USDC:USDC", "market_type": "perpetual",
        "side": "sell", "order_type": "market", "size": fmt(eth_size), "min_price": fmt(eth_sell_min),
        "max_price": fmt(eth_sell_max),
        "time_in_force": "ioc", "max_notional_usdc": "12", "max_fee_usdc": fmt(max_fee_usdc),
        "max_realized_loss_usdc": fmt(max_loss_usdc), "reduce_only": True,
    }, perp_scope))
    specs.append(("hyperliquid.account.set_leverage", "restore-eth-leverage", {
        "request_id": request_id("restore"), "account": account, "symbol": "ETH/USDC:USDC", "leverage": leverage, "cross": cross,
    }, exact_scope([], [], [])))
    withdrawal = {
        "request_id": request_id("withdraw"), "account": account, "asset": "USDC", "amount": "5",
        "address": wallet_address.lower(), "max_amount_usdc": "5", "max_fee_usdc": "1",
    }
    withdrawal_debits = [{"resource": eth_resource, "amount": amount(USDC_HL, Decimal("5"))}]
    withdrawal_costs = [cost(eth_resource, USDC_HL, Decimal("1"), "included_in_debit")]
    withdrawal_effects = [{"kind": "transfer", "from": eth_resource,
                           "to": f"ethereum:42161:{wallet_address.lower()}",
                           "debit_asset": USDC_HL, "credit_asset": USDC_ARB,
                           "maximum_debit": atoms(Decimal("5")), "minimum_credit": "4000000"}]
    specs.append(("hyperliquid.account.withdraw", "withdraw-5-usdc-to-arbitrum", withdrawal,
                  exact_scope(withdrawal_debits, withdrawal_costs, withdrawal_effects)))
    all_debits = [debit for _, _, _, scope in specs for debit in scope["maximum_debits"]]
    all_costs = [bound for _, _, _, scope in specs for bound in scope["maximum_costs"]]
    aggregate_debits = aggregate_amounts(all_debits)
    aggregate_cost_bounds = aggregate_costs(all_costs)
    completions = {
        "hype-limit-buy": {"minimum_net_receipt": {
            "resource": spot_resource,
            "amount": {"asset": base_asset, "atoms": atoms_floor(hype_size - max_fee_usdc / buy_min)},
        }, "operation_succeeded": True},
        "hype-bounded-market-sell": {"minimum_net_receipt": {
            "resource": spot_resource,
            "amount": {"asset": USDC_HL, "atoms": atoms_floor(max(Decimal(0), hype_size * sell_min - max_fee_usdc))},
        }, "operation_succeeded": True},
    }
    plan = {
        "version": 1,
        "name": "Hyperliquid HYPE and ETH bounded verification",
        "steps": [step(
            step_id, operation, input_value, scope,
            completions.get(step_id, {"minimum_net_receipt": {
                "resource": f"ethereum:42161:{wallet_address.lower()}",
                "amount": {"asset": USDC_ARB, "atoms": "4000000"},
            }, "operation_succeeded": True} if operation == "hyperliquid.account.withdraw" else None),
        ) for (operation, step_id, input_value, scope) in specs],
        "aggregate_bounds": {
            "maximum_debits": aggregate_debits,
            "maximum_costs": aggregate_cost_bounds,
        },
    }
    # The plan carries only the explicit native fee ceiling. Fin currently has
    # no separate current-fee quote, so the handoff labels that limit as a cap,
    # not an estimate or verification of the live bridge fee.
    return plan, {"status": "bounded_proposal_current_fee_unverified", "input": withdrawal,
                  "destination_asset": USDC_ARB, "gross_amount_usdc": "5", "maximum_fee_usdc": "1",
                  "minimum_net_receipt_usdc": "4",
                  "limitation": "The caller-bound 1 USDC fee cap is checked by Fin; no current Bridge2 fee quote is exposed by Fin reads."}


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    path.chmod(0o600)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fin", default="fin")
    parser.add_argument("--account", required=True, help="Existing EVM wallet name or explicit Hyperliquid connection alias")
    parser.add_argument("--wallet", required=True, help="Exact enrolled Arbitrum wallet alias")
    parser.add_argument("--wallet-address", required=True, help="Full human-reviewed Arbitrum destination")
    parser.add_argument("--max-fee-usdc", default="0.03")
    parser.add_argument("--max-realized-loss-usdc", default="0.50")
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()
    if not ADDRESS.fullmatch(args.wallet_address) or args.wallet_address.lower() == "0x" + "0" * 40:
        parser.error("--wallet-address must be a full nonzero EVM address")
    if args.out_dir.exists():
        parser.error("--out-dir must be new and unused")
    try:
        args.max_fee = exact_decimal(args.max_fee_usdc, "--max-fee-usdc", positive=True)
        args.max_loss = exact_decimal(args.max_realized_loss_usdc, "--max-realized-loss-usdc")
        if args.max_loss < 0:
            raise PlanError("--max-realized-loss-usdc must be nonnegative")
    except PlanError as exc:
        parser.error(str(exc))
    return args


def main() -> int:
    args = parse_args()
    try:
        evidence = gather(args.fin, args.account, args.wallet, args.wallet_address)
        evidence["_fin"] = args.fin
        plan, followup = build_plan(evidence, args.account, args.wallet_address, args.max_fee, args.max_loss)
        result = invoke(args.fin, ["plans", "preview", "--input", compact(plan), "--valid-for", "10m", "--json"], {"plans"})
        if not isinstance(result.get("review_hash"), str) or not re.fullmatch(r"[0-9a-fA-F]{64}", result["review_hash"]):
            raise PlanError("plans preview omitted a valid review hash")
        followup["plan_preview"] = result
        args.out_dir.mkdir(mode=0o700, parents=True)
        write_json(args.out_dir / "plan.json", plan)
        write_json(args.out_dir / "plan-preview.json", result)
        write_json(args.out_dir / "withdrawal-followup.json", followup)
        write_json(args.out_dir / "read-evidence.json", {key: value for key, value in evidence.items() if key != "_fin"})
        (args.out_dir / "human-handoff.md").write_text(handoff(args, result["review_hash"]), encoding="utf-8")
        print(f"Read-only review artifacts prepared in {args.out_dir.resolve()}")
        print("No authorization or financial execution command was run.")
        return 0
    except PlanError as exc:
        print(f"Preparation stopped: {exc}", file=sys.stderr)
        return 2


def handoff(args: argparse.Namespace, review_hash: str) -> str:
    out = shlex.quote(str(args.out_dir.resolve()))
    return f"""# Human review required — Hyperliquid verification

This packet was built from Fin reads for Hyperliquid `{args.account}` and Arbitrum wallet `{args.wallet}` (`{args.wallet_address.lower()}`). Plan review hash: `{review_hash}`. The helper ran only `fin operations`, `fin snapshot`, `fin read`, and the read-only `fin plans preview`. It did not authorize, execute, enroll, install, or start Fin.

Review every input, plan preview and exact debit/cost identity in `{out}/`. Plan consent does not guarantee fills. Stop if either HYPE order has any partial or zero fill, even if its receipt floor reports step completion; inspect the native filled quantity before continuing. The HYPE IOC buy and bounded market sell each also require their minimum receipts for the fixed quantity. The sell's worst-case base-denominated fees are covered only by the verified starting HYPE dust check. ETH starts flat and opens and closes at the same fixed quantity with a reduce-only close. Stop on any partial close or if the resulting position is not flat; only then proceed to restore the observed leverage/margin mode or withdraw. This plan does not prove full restoration.

Immediately before each order submission, fetch a fresh Fin order book and compare it with the frozen limits: each buy's `max_price` must remain no higher than the current ask × 1.005, and each sell's `min_price` must remain no lower than the current bid × 0.995. The plan keeps the prices from its creation-time reads; it does not move them when markets change. Stop and obtain a new preview and human approval if a comparison fails.

The final step proposes a 5 USDC gross Arbitrum withdrawal, with a 1 USDC maximum fee and 4 USDC minimum receipt. This is only the caller-bound fee ceiling; Fin does not expose a separate current Bridge2 fee quote, so do not read it as a fee estimate or verification. The final withdrawal must be freshly reviewed after all preceding steps settle. Consolidation transfers must also be separately sized from a fresh spot/perpetual balance read; this helper will not guess future proceeds.

To refresh the named-plan preview (read-only):

```sh
fin plans preview --input "$(cat {out}/plan.json)" --valid-for 10m --json
```

Only a human may decide whether to authorize this exact reviewed plan in their terminal. Any changed input, balance, position, fee, market or connection requires new reads and a new preview.
"""


if __name__ == "__main__":
    raise SystemExit(main())
