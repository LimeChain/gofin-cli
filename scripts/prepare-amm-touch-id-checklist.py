#!/usr/bin/env python3
"""Prepare read-only AMM acceptance artifacts; never authorize or execute."""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
import time
import uuid
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any


INPUT_AMOUNT = "0.001"
SLIPPAGE_BPS = 50
MAX_SLIPPAGE_BPS = 100
MAX_IMPACT_BPS = 100
MAX_DEADLINE_SECONDS = 600
MAX_MOVEMENT_USD = Decimal("10")
MAX_TOTAL_COST_USD = Decimal("20")
GROSS_LIMIT_USD = Decimal("100")
ADDRESS = re.compile(r"^0x[0-9a-fA-F]{40}$")
ALLOWED_READS = {"wallet.token", "wallet.balances", "uniswap.quote"}
ALLOWED_READS.update({"ramses.quote", "sushi.quote", "pancakeswap.quote"})
SWAP_OPERATIONS = {"uniswap.swap", "ramses.swap", "sushi.swap", "pancakeswap.swap"}
FEE_TIERS = {"uniswap": (100, 500, 3000, 10000), "sushi": (100, 500, 3000, 10000),
             "pancakeswap": (100, 500, 2500, 10000)}
V3_NETWORKS = {"sushi": (1, 42161), "pancakeswap": (1, 42161, 4663)}


class ChecklistError(Exception):
    pass


def compact(value: Any) -> str:
    return json.dumps(value, separators=(",", ":"), sort_keys=True)


def invoke(fin: str, argv: list[str], allowed: set[str]) -> dict[str, Any]:
    """Run only a whitelisted Fin read/preview command; never use a shell."""
    if len(argv) >= 2 and argv[0] == "read" and argv[1] in allowed:
        command = f"read {argv[1]}"
    elif (len(argv) >= 2 and argv[0] == "preview"
          and argv[1] in SWAP_OPERATIONS and argv[1] in allowed):
        command = f"preview {argv[1]}"
    elif len(argv) >= 2 and argv[:2] == ["plans", "preview"] and "plans" in allowed:
        command = "plans preview"
    else:
        raise ChecklistError("internal refusal: command is not read/preview-only")
    try:
        result = subprocess.run(
            [fin, *argv], check=False, capture_output=True, text=True, timeout=180
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ChecklistError(f"Fin {command} did not complete: {exc}") from exc
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic"
        raise ChecklistError(f"Fin {command} failed ({result.returncode}): {detail}")
    try:
        parsed = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ChecklistError(f"Fin {command} did not return valid JSON") from exc
    if not isinstance(parsed, dict):
        raise ChecklistError(f"Fin {command} returned an unexpected JSON shape")
    return parsed


def read(fin: str, operation: str, payload: dict[str, Any]) -> dict[str, Any]:
    if operation not in ALLOWED_READS:
        raise ChecklistError("internal refusal: unsupported Fin read")
    return invoke(fin, ["read", operation, "--input", compact(payload), "--json"], ALLOWED_READS)


def preview(fin: str, operation: str, payload: dict[str, Any]) -> dict[str, Any]:
    if operation not in SWAP_OPERATIONS:
        raise ChecklistError("internal refusal: unsupported Fin preview")
    return invoke(fin, ["preview", operation, "--input", compact(payload), "--json"], {operation})


def plan_preview(fin: str, plan: dict[str, Any]) -> dict[str, Any]:
    return invoke(
        fin,
        ["plans", "preview", "--input", compact(plan), "--valid-for", "10m", "--json"],
        {"plans"},
    )


def data(result: dict[str, Any]) -> dict[str, Any]:
    value = result.get("data")
    if isinstance(value, dict):
        return value
    # A few older brokers return the read payload directly.
    return result


def atoms(value: Any, label: str) -> int:
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, str) and value.isdecimal():
        return int(value)
    raise ChecklistError(f"{label} is missing or is not an exact nonnegative atom count")


def decimal_atoms(value: int, decimals: int) -> str:
    if decimals < 0 or decimals > 255:
        raise ChecklistError("token metadata contains invalid decimals")
    scale = 10**decimals
    whole, fraction = divmod(value, scale)
    if not fraction:
        return str(whole)
    return f"{whole}.{fraction:0{decimals}d}".rstrip("0").rstrip(".")


def exact_fee_cap(raw: str) -> str:
    try:
        amount = Decimal(raw)
    except InvalidOperation as exc:
        raise ChecklistError("--fee-cap must be a positive decimal ETH amount") from exc
    if not amount.is_finite() or amount <= 0 or amount.as_tuple().exponent < -18:
        raise ChecklistError("--fee-cap must be positive, finite, and have at most 18 decimals")
    return format(amount, "f")


def resource_address(resource: str, chain_id: int) -> str:
    prefix = f"ethereum:{chain_id}:"
    if not resource.startswith(prefix):
        raise ChecklistError("wallet.balances did not identify the selected Ethereum resource")
    address = resource[len(prefix) :]
    if not ADDRESS.fullmatch(address):
        raise ChecklistError("wallet resource did not contain a verified EVM address")
    return address


def require_quote_evidence(quote: dict[str, Any]) -> tuple[int, int, int]:
    if (not quote.get("observed_at") or quote.get("price_impact_bps") is None
            or quote.get("block_number") is None or not quote.get("block_hash")):
        raise ChecklistError("quote lacks timestamp, block, or price-impact evidence")
    observed = quote["observed_at"]
    # Reject malformed timestamps; the broker already enforces quote freshness.
    try:
        import datetime as dt

        timestamp = dt.datetime.fromisoformat(observed.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError, TypeError) as exc:
        raise ChecklistError("quote timestamp is not parseable") from exc
    age = time.time() - timestamp
    if age < -5 or age > 120:
        raise ChecklistError("quote is future-dated or older than the 120-second review window")
    impact = int(quote["price_impact_bps"])
    if impact < 0 or impact > MAX_IMPACT_BPS:
        raise ChecklistError(f"quote price impact {impact} bps exceeds the 100 bps acceptance cap")
    input_atoms = atoms(quote.get("input"), "quote input")
    output_atoms = atoms(quote.get("output"), "quote output")
    if input_atoms == 0 or output_atoms == 0:
        raise ChecklistError("quote input and output must both be positive")
    return input_atoms, output_atoms, impact


def operation_names(dex: str) -> tuple[str, str]:
    if dex not in {"uniswap", "ramses", "sushi", "pancakeswap"}:
        raise ChecklistError("unsupported DEX")
    return f"{dex}.quote", f"{dex}.swap"


def validate_ramses_quote(quote: dict[str, Any], tick_spacing: int,
                          max_pool_fee_ppm: int | None) -> int:
    if quote.get("dex") != "ramses" or quote.get("protocol") != "v3":
        raise ChecklistError("quote did not identify the requested Ramses v3 route")
    if quote.get("tick_spacing") != tick_spacing:
        raise ChecklistError("quote tick spacing did not match the requested Ramses pool")
    fee = quote.get("swap_fee_ppm")
    if not isinstance(fee, int) or isinstance(fee, bool) or fee < 0 or fee >= 1_000_000:
        raise ChecklistError("Ramses quote omitted a valid observed pool fee")
    if max_pool_fee_ppm is not None and (max_pool_fee_ppm < fee or max_pool_fee_ppm >= 1_000_000):
        raise ChecklistError("--max-pool-fee-ppm must be at least the quoted fee and below 1000000")
    return max_pool_fee_ppm if max_pool_fee_ppm is not None else fee


def usd_micros(result: dict[str, Any], path: str) -> int:
    obj: Any = result
    for field in path.split("."):
        obj = obj.get(field) if isinstance(obj, dict) else None
    if isinstance(obj, int) and obj >= 0:
        return obj
    if isinstance(obj, str) and obj.isdecimal():
        return int(obj)
    raise ChecklistError(f"preview omitted verified USD valuation {path}; refusing acceptance artifact")


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    path.chmod(0o600)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fin", default="fin", help="Fin executable (default: fin)")
    parser.add_argument("--wallet", required=True, help="Existing Fin wallet connection name")
    parser.add_argument("--chain-id", required=True, type=int)
    parser.add_argument("--token-out", required=True, help="Exact caller-reviewed output token contract")
    parser.add_argument("--protocol", required=True, choices=("v3", "v4"))
    parser.add_argument("--dex", default="uniswap", choices=("uniswap", "ramses", "sushi", "pancakeswap"))
    parser.add_argument("--fee-cap", required=True, help="Maximum native transaction fee in ETH")
    parser.add_argument("--max-pool-fee-ppm", type=int,
                        help="Optional Ramses quote-time pool-fee ceiling in ppm")
    route = parser.add_mutually_exclusive_group(required=False)
    route.add_argument("--fee-tier", type=int, help="Reviewed v3 fee tier in ppm for the selected DEX")
    route.add_argument("--pool-key", help="Complete caller-reviewed Uniswap v4 pool-key JSON object")
    parser.add_argument("--tick-spacing", type=int, help="Reviewed Ramses v3 tick spacing")
    parser.add_argument("--network", default="evm", choices=("evm", "ethereum"))
    parser.add_argument("--out-dir", required=True, type=Path, help="New directory for unsigned review artifacts")
    args = parser.parse_args()
    if args.chain_id <= 0 or not ADDRESS.fullmatch(args.token_out):
        parser.error("--chain-id must be positive and --token-out must be an exact EVM address")
    if args.dex != "ramses" and args.tick_spacing is not None:
        parser.error("--tick-spacing is only valid with --dex ramses")
    if args.dex in V3_NETWORKS and (args.chain_id not in V3_NETWORKS[args.dex] or args.protocol != "v3"):
        parser.error("Selected DEX requires v3 and a reviewed network")
    if args.dex == "ramses":
        if args.chain_id != 4663 or args.protocol != "v3":
            parser.error("Ramses acceptance is currently reviewed only for v3 on chain 4663")
        if args.tick_spacing not in (1, 5, 10, 50, 100, 200) or args.fee_tier is not None or args.pool_key is not None:
            parser.error("Ramses v3 requires a supported --tick-spacing and forbids Uniswap pool selectors")
        if args.max_pool_fee_ppm is not None and not 0 <= args.max_pool_fee_ppm < 1_000_000:
            parser.error("--max-pool-fee-ppm must be between 0 and 999999")
    elif args.protocol == "v3" and (args.fee_tier is None or args.pool_key is not None):
        parser.error("v3 requires --fee-tier and forbids --pool-key")
    elif args.protocol == "v4" and (args.pool_key is None or args.fee_tier is not None):
        parser.error("Uniswap v4 requires --pool-key and forbids --fee-tier")
    if args.dex in FEE_TIERS and args.fee_tier is not None and args.fee_tier not in FEE_TIERS[args.dex]:
        parser.error(f"--fee-tier must be a reviewed {args.dex} tier: {FEE_TIERS[args.dex]}")
    if args.dex != "ramses" and args.max_pool_fee_ppm is not None:
        parser.error("--max-pool-fee-ppm is only valid with --dex ramses")
    if args.out_dir.exists():
        parser.error("--out-dir must be a new, unused directory")
    try:
        args.max_fee_eth = exact_fee_cap(args.fee_cap)
        args.pool_key_value = json.loads(args.pool_key) if args.pool_key else None
    except (ChecklistError, json.JSONDecodeError) as exc:
        parser.error(str(exc))
    if args.protocol == "v4" and not isinstance(args.pool_key_value, dict):
        parser.error("--pool-key must be a JSON object")
    return args


def main() -> int:
    args = parse_args()
    common = {"wallet": args.wallet, "network": args.network, "chain_id": args.chain_id}
    token_out = args.token_out.lower()
    token_result = read(args.fin, "wallet.token", {**common, "token": token_out})
    token = data(token_result)
    token_ref = str(token.get("reference", token.get("token", ""))).lower()
    decimals = token.get("decimals")
    if token_ref != token_out or not isinstance(decimals, int) or not 0 <= decimals <= 255:
        raise ChecklistError("pinned wallet.token metadata did not match the exact requested output token")

    balances_result = read(args.fin, "wallet.balances", {**common, "tokens": [token_out]})
    physical_resource = balances_result.get("resource")
    if not isinstance(physical_resource, str):
        raise ChecklistError("wallet.balances omitted the physical wallet resource")
    recipient = resource_address(physical_resource, args.chain_id)

    quote_input = {
        **common,
        "token_in": "native",
        "token_out": token_out,
        "amount": INPUT_AMOUNT,
    }
    quote_operation, swap_operation = operation_names(args.dex)
    if args.dex == "ramses":
        quote_input["tick_spacing"] = args.tick_spacing
    elif args.protocol == "v3":
        quote_input["protocol"] = args.protocol
        quote_input["fee_tier"] = args.fee_tier
    else:
        quote_input["protocol"] = args.protocol
        quote_input["v4_pool_key"] = args.pool_key_value
    quote_result = read(args.fin, quote_operation, quote_input)
    quote = data(quote_result)
    if str(quote.get("protocol")) != args.protocol:
        raise ChecklistError("quote protocol did not match the selected protocol")
    if args.dex == "ramses":
        max_pool_fee_ppm = validate_ramses_quote(quote, args.tick_spacing, args.max_pool_fee_ppm)
    elif args.dex in V3_NETWORKS and quote.get("dex") != args.dex:
        raise ChecklistError("quote DEX did not match the selected deployment")
    if args.dex != "ramses" and args.protocol == "v3" and int(quote.get("fee_tier", -1)) != args.fee_tier:
        raise ChecklistError("quote fee tier did not match the requested v3 tier")
    input_atoms, output_atoms, impact = require_quote_evidence(quote)
    quoted_in = quote.get("token_in", {})
    quoted_out = quote.get("token_out", {})
    if quoted_in.get("reference") != "native" or str(quoted_out.get("reference", "")).lower() != token_out:
        raise ChecklistError("quote asset identities did not match the requested route")
    if quoted_out.get("decimals") != decimals:
        raise ChecklistError("quote output decimals did not match pinned token metadata")

    if input_atoms != 10**15 or quoted_in.get("decimals") != 18:
        raise ChecklistError("quote input is not exactly 0.001 native ETH")
    min_output_atoms = (output_atoms * (10_000 - SLIPPAGE_BPS) + 9_999) // 10_000
    min_output = decimal_atoms(min_output_atoms, decimals)
    swap_request = {
        **common,
        "request_id": str(uuid.uuid4()),
        "max_fee_eth": args.max_fee_eth,
        "token_in": "native",
        "token_out": token_out,
        "amount": INPUT_AMOUNT,
        "recipient": recipient,
        "min_output": min_output,
        "slippage_bps": SLIPPAGE_BPS,
        "max_price_impact_bps": MAX_IMPACT_BPS,
        "deadline": int(time.time()) + MAX_DEADLINE_SECONDS,
    }
    if args.dex == "ramses":
        swap_request["tick_spacing"] = args.tick_spacing
        swap_request["max_pool_fee_ppm"] = max_pool_fee_ppm
    elif args.protocol == "v3":
        swap_request["protocol"] = args.protocol
        swap_request["fee_tier"] = args.fee_tier
    else:
        swap_request["protocol"] = args.protocol
        swap_request["v4_pool_key"] = args.pool_key_value
    swap_preview = preview(args.fin, swap_operation, swap_request)
    if swap_preview.get("operation") != swap_operation or not isinstance(swap_preview.get("intent"), dict):
        raise ChecklistError("swap preview omitted its operation intent")
    movement = usd_micros(swap_preview, "valuation.movement_usd_micros")
    usd_micros(swap_preview, "valuation.maximum_fee_usd_micros")
    prices = swap_preview["valuation"].get("prices", [])
    native_prices = [price for price in prices if price.get("asset") ==
                     {"network": f"ethereum:{args.chain_id}", "reference": "native", "decimals": 18}]
    if len(native_prices) != 1:
        raise ChecklistError("preview omitted its exact native fee valuation identity")
    native_price = native_prices[0]
    unit_atoms = atoms(native_price.get("unit_atoms"), "native price units")
    price_micros = atoms(native_price.get("usd_micros_per_unit"), "native price micros")
    if unit_atoms != 10**18 or price_micros == 0:
        raise ChecklistError("native fee valuation does not identify one whole ETH")
    fee_atoms = int(Decimal(args.max_fee_eth) * 10**18)
    fees = (fee_atoms * price_micros + unit_atoms - 1) // unit_atoms
    if Decimal(movement + fees) / 1_000_000 > MAX_MOVEMENT_USD:
        raise ChecklistError("swap movement plus maximum costs exceeds the $10 acceptance ceiling")
    if Decimal(fees) / 1_000_000 > MAX_TOTAL_COST_USD:
        raise ChecklistError("quoted native fee cap exceeds the $20 total-cost ceiling")
    if impact > MAX_IMPACT_BPS:
        raise ChecklistError("fresh quote exceeds the maximum permitted impact")

    bounds = {
        "max_input": INPUT_AMOUNT,
        "max_fee_eth": args.max_fee_eth,
        "max_slippage_bps": MAX_SLIPPAGE_BPS,
        "max_price_impact_bps": MAX_IMPACT_BPS,
        "minimum_rate": {"output_atoms": str(output_atoms * 98), "input_atoms": str(input_atoms * 100)},
        "absolute_minimum_output": None,
        "allowed_fee_tiers": [args.fee_tier] if args.dex in FEE_TIERS and args.protocol == "v3" else [],
        "max_deadline_seconds": MAX_DEADLINE_SECONDS,
    }
    intent = swap_preview["intent"]
    debits = intent.get("debits")
    costs = intent.get("maximum_costs")
    if not isinstance(debits, list) or not debits or not isinstance(costs, list) or not costs:
        raise ChecklistError("swap preview omitted typed debit or maximum-cost identities")
    # The plan's aggregate cost authority must cover the declared variation
    # cap, rather than just today's smaller estimated executable envelope.
    costs = json.loads(json.dumps(costs))
    if len(costs) != 1 or costs[0]["amount"]["asset"]["reference"] != "native":
        raise ChecklistError("swap cost identity is not one exact native fee")
    costs[0]["amount"]["atoms"] = str(int(Decimal(args.max_fee_eth) * 10**18))
    plan = {
        "version": 1,
        "name": f"Reviewed {args.dex} {args.protocol} native-to-token acceptance swap",
        "steps": [{
            "step_id": "forward-swap",
            "operation": swap_operation,
            "input": swap_request,
            "permitted_variations": bounds,
            "completion": {"operation_succeeded": True},
            "funding_dependencies": [],
        }],
        "aggregate_bounds": {"maximum_debits": debits, "maximum_costs": costs},
    }
    plan_review = plan_preview(args.fin, plan)
    review_hash = plan_review.get("review_hash")
    if not isinstance(review_hash, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", review_hash):
        raise ChecklistError("plans preview omitted a valid review hash")

    out = args.out_dir
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    artifacts = {
        "inputs.json": {
            "token_request": {**common, "token": token_out},
            "balances_request": {**common, "tokens": [token_out]},
            "quote_request": quote_input,
        },
        "token.json": token_result,
        "balances.json": balances_result,
        "quote.json": quote_result,
        "swap-input.json": swap_request,
        "swap-bounds.json": bounds,
        "swap-preview.json": swap_preview,
        "plan.json": plan,
        "plan-preview.json": plan_review,
    }
    try:
        for filename, value in artifacts.items():
            write_json(out / filename, value)
        checklist = build_checklist(args, recipient, min_output, movement, fees, impact, review_hash, swap_operation)
        (out / "checklist.md").write_text(checklist, encoding="utf-8")
    except OSError:
        # Preserve the out-dir no-overwrite rule; partial review material is still safe and unsigned.
        raise
    print(f"Read-only review artifacts prepared in {out.resolve()}")
    print("No authorization or financial execution command was run.")
    return 0


def build_checklist(args: argparse.Namespace, recipient: str, minimum: str,
                    movement: int, fees: int, impact: int, review_hash: str,
                    swap_operation: str = "uniswap.swap") -> str:
    path = shlex.quote(str(args.out_dir.resolve()))
    plan_id = str(uuid.uuid4())
    boundary_id = str(uuid.uuid4())
    dex = getattr(args, "dex", "uniswap")
    quote_operation, _ = operation_names(dex)
    pool_fee = getattr(args, "max_pool_fee_ppm", None)
    route = (f"Ramses v3, tick spacing {args.tick_spacing}, maximum quoted pool fee "
             f"{pool_fee if pool_fee is not None else 'the initially quoted fee'} ppm"
             if dex == "ramses" else f"{ {'uniswap':'Uniswap', 'sushi':'Sushi', 'pancakeswap':'PancakeSwap'}[dex]} {args.protocol}")
    return f"""# Human Touch ID handoff — review required

Generated from pinned read-only Fin results for {route} on chain `{args.chain_id}`.
Recipient is the wallet resource `{recipient}`. Quoted impact was {impact} bps;
selected slippage is 50 bps and the rounded minimum output is `{minimum}`.
Estimated operation debit is ${movement / 1_000_000:.6f}; maximum-cost valuation
under the supplied native fee cap is ${fees / 1_000_000:.6f}.

This folder contains unsigned JSON only. The script ran `wallet.token`,
`wallet.balances`, `{quote_operation}`, `preview {swap_operation}`, and `plans preview`.
It did not authorize, execute, install, enroll credentials, or broadcast.
Read the exact token, pool/router, block evidence, recipient, fee cap, and every
generated JSON file before any human action. A quote is useful for at most 120
seconds and the transaction deadline is at most 600 seconds. Any delay, changed
route, new quote, or changed amount requires regenerating and reviewing every
artifact and obtaining fresh consent.

## Required acceptance controls

- Maximum movement: **$10** per operation; maximum fee and cleanup ledger: **$20**
  total; rolling gross allocation: **$100** across all networks, protocols,
  retries, approvals, and cleanup. Verify `fin policy show` first. Never raise
  these limits for this test.
- The generated plan contains only the forward swap. It is not approval to
  proceed until a complete return path is separately planned, funded, priced,
  and shown to restore initial balances/exposure and any changed allowances,
  net of unavoidable costs. Account for approval, failed-transaction, retry,
  and cleanup gas within the same $20 ledger.
- For rollups, use Fin's canonical sequencer-inclusion settlement result. This
  checklist does not require a later finalized-tag wait.

## Fresh review commands (read-only)

```sh
fin read wallet.token --input "$(jq -c '.token_request' {path}/inputs.json)" --json
fin read wallet.balances --input "$(jq -c '.balances_request' {path}/inputs.json)" --json
fin read {quote_operation} --input "$(jq -c '.quote_request' {path}/inputs.json)" --json
fin preview {swap_operation} --input "$(cat {path}/swap-input.json)" --json
fin plans preview --input "$(cat {path}/plan.json)" --valid-for 10m --json
```

The first three read requests are in `inputs.json` as one review artifact. Copy
the selected request object into a temporary file if you want to rerun it; the
observations in `token.json`, `balances.json`, and `quote.json` are results, not
inputs. Never copy result JSON back as input.

## Human-only write choices

Choose one execution path after a fresh review. These lines are examples for
the human's own terminal; this generator never runs them.

**Standalone exact approval:**

```sh
fin execute {swap_operation} --input "$(cat {path}/swap-input.json)" --id {uuid.uuid4()}
```

**Boundary authorization** (approval once, then separately invoke each concrete attempt):

```sh
fin authorizations authorize {swap_operation} --id {boundary_id} --input "$(cat {path}/swap-input.json)" --bounds "$(cat {path}/swap-bounds.json)" --valid-for 10m --json > {path}/boundary-approval.json
AUTHORIZATION_ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' {path}/boundary-approval.json)
fin execute {swap_operation} --input "$(cat {path}/swap-input.json)" --id {uuid.uuid4()} --authorization "$AUTHORIZATION_ID"
fin authorizations status "$AUTHORIZATION_ID"
fin authorizations cancel "$AUTHORIZATION_ID"
```

**Named plan** (the current review hash binds the exact reviewed plan):

```sh
fin plans authorize --input "$(cat {path}/plan.json)" --id {plan_id} --review-hash {review_hash} --valid-for 10m --json > {path}/plan-approval.json
PLAN_ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["plan_id"])' {path}/plan-approval.json)
fin execute {swap_operation} --input "$(cat {path}/swap-input.json)" --id {uuid.uuid4()} --plan "$PLAN_ID" --step forward-swap
fin plans status "$PLAN_ID"
fin plans cancel "$PLAN_ID"
```

Review CLI help and schemas before use. A denied or cancelled approval grants
no authority. Expired, out-of-bounds, duplicate, or uncertain attempts must be
checked with `fin status` / the relevant authorization or plan status; never
retry with a new identity until the old outcome is reconciled. Cleanup and
allowance restoration are separate writes requiring their own valid authority
and must be included in the return-path and cost plan.
"""


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ChecklistError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(2)
