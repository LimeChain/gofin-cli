#!/usr/bin/env python3
"""Exercise Aave MCP read discovery through the public CLI and dev broker.

All broker runs are synthetic, credential-free, and use an in-process
deterministic MCP transport. No Ethereum RPC or account data is accessed.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid


ROOT = Path(__file__).resolve().parents[1]
DEV = ROOT / "scripts" / "gofin-dev.py"
USER = "0x1111111111111111111111111111111111111111"


def run(args: list[str], env: dict[str, str], *, expected: int = 0) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(args, cwd=ROOT, env=env, capture_output=True, text=True, timeout=180)
    if result.returncode != expected:
        raise AssertionError(
            f"{' '.join(args)} exited {result.returncode}, expected {expected}: "
            f"stdout={result.stdout!r} stderr={result.stderr!r}"
        )
    return result


def json_command(args: list[str], env: dict[str, str], *, expected: int = 0) -> dict:
    result = run(args, env, expected=expected)
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise AssertionError(f"Command did not return JSON: {result.stdout!r} {result.stderr!r}") from error


def cli(env: dict[str, str], name: str, *args: str, expected: int = 0) -> dict:
    return json_command(
        [sys.executable, str(DEV), "cli", name, "--", *args], env, expected=expected
    )


def start(env: dict[str, str], name: str, fixture: str) -> None:
    json_command([
        sys.executable, str(DEV), "--json", "start", name,
        "--mode", "synthetic", "--fixture", fixture,
    ], env)


def stop(env: dict[str, str], name: str) -> None:
    json_command([sys.executable, str(DEV), "--json", "stop", name], env)


def read(env: dict[str, str], name: str, operation: str, arguments: dict, *, expected: int = 0) -> dict:
    return cli(
        env, name, "read", operation, "--input", json.dumps(arguments), "--json",
        expected=expected,
    )


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def run_fixture(env: dict[str, str], name: str, fixture: str) -> None:
    start(env, name, fixture)
    try:
        # The CLI may explicitly bootstrap during this command. The broker's
        # raw Capabilities protocol method remains free of implicit HTTP calls.
        before = cli(env, name, "capabilities", "--json")
        operations = before.get("operations", [])
        require("aave.mcp.tools" in {row.get("name") for row in operations},
                "offline catalog lacks the static Aave MCP bootstrap read")
        static = cli(env, name, "explain", "aave.mcp.tools", "--json")
        require(static.get("name") == "aave.mcp.tools", "static bootstrap explanation is unavailable")

        if fixture == "aave-mcp-read-stale-catalog":
            first_catalog = read(env, name, "aave.mcp.tools", {})
            require(first_catalog.get("provenance") == "unverified_official_mcp",
                    f"initial catalog could not be discovered: {first_catalog}")
            failed_catalog = read(env, name, "aave.mcp.tools", {}, expected=1)
            require(failed_catalog.get("error", {}).get("code") == "AAVE_MCP_READ_UNAVAILABLE",
                    f"catalog outage after success did not fail the explicit read: {failed_catalog}")
            after_catalog_failure = cli(env, name, "capabilities", "--json")
            names = {row.get("name") for row in after_catalog_failure.get("operations", [])}
            require("aave.mcp.tools" in names and "aave.mcp.get_user_positions" not in names,
                    f"stale dynamic definitions were exposed as current after catalog outage: {names}")
            require(after_catalog_failure.get("aave_mcp", {}).get("status") == "unavailable",
                    f"capabilities did not report the catalog outage: {after_catalog_failure.get('aave_mcp')}")
            failed_position = read(
                env, name, "aave.mcp.get_user_positions", {"user": USER}, expected=1,
            )
            require(failed_position.get("error", {}).get("code") == "AAVE_MCP_READ_UNAVAILABLE",
                    f"position read did not fail closed after catalog outage: {failed_position}")
            return

        if fixture == "aave-mcp-read-unavailable":
            failure = read(env, name, "aave.mcp.tools", {}, expected=1)
            require(failure.get("error", {}).get("code") == "AAVE_MCP_READ_UNAVAILABLE",
                    f"catalog outage did not return the public read error: {failure}")
            # A transient upstream outage leaves local discovery available.
            after = cli(env, name, "capabilities", "--json")
            require("aave.mcp.tools" in {row.get("name") for row in after.get("operations", [])},
                    "offline catalog disappeared after upstream read outage")
            require(after.get("aave_mcp", {}).get("status") == "unavailable",
                    f"capabilities did not report upstream outage: {after.get('aave_mcp')}")
            return

        catalog = read(env, name, "aave.mcp.tools", {})
        require(catalog.get("provenance") == "unverified_official_mcp",
                f"catalog provenance is missing: {catalog}")
        names = {tool.get("name") for tool in catalog.get("tools", [])}
        require({
            "get_user_positions", "get_user_rewards", "get_new_fixture_metric",
            "get_fixture_call_count", "get_fixture_request_count",
        } <= names,
                f"safe annotated read tools were not exposed: {names}")
        require(not ({"prepare_liquidation", "get_user_positions_unannotated", "get_credentials"} & names),
                f"unsafe/unannotated tool leaked into the public catalog: {names}")

        after = cli(env, name, "capabilities", "--json")
        operations = {row.get("name"): row for row in after.get("operations", [])}
        for name_ in (
            "aave.mcp.get_user_positions", "aave.mcp.get_user_rewards",
            "aave.mcp.get_new_fixture_metric", "aave.mcp.get_fixture_call_count",
            "aave.mcp.get_fixture_request_count",
        ):
            require(name_ in operations, f"explicit bootstrap did not add {name_} to capabilities")
            require(operations[name_].get("kind") == "read", f"dynamic tool was not read-only: {operations[name_]}")
        explanation = cli(env, name, "explain", "aave.mcp.get_user_positions", "--json")
        require(explanation.get("semantics", {}).get("provenance") == "unverified_official_mcp",
                f"dynamic explanation omitted unverified provenance: {explanation}")
        schema = cli(env, name, "schema", "aave.mcp.get_user_positions", "--json")
        require(schema.get("input", {}).get("required") == ["user"],
                f"dynamic schema is not visible through the CLI: {schema}")

        calls_before_invalid = read(env, name, "aave.mcp.get_fixture_call_count", {})
        invalid_before_count = calls_before_invalid.get("result", {}).get("structuredContent", {}).get("data", {}).get("tool_calls")
        invalid = read(env, name, "aave.mcp.get_user_positions", {"user": 7}, expected=1)
        require(invalid.get("error", {}).get("code") == "AAVE_MCP_READ_INPUT_INVALID",
                f"invalid schema arguments were not rejected: {invalid}")
        denied = read(env, name, "aave.mcp.prepare_liquidation", {}, expected=1)
        require(denied.get("error", {}).get("code") == "AAVE_MCP_READ_UNAVAILABLE",
                f"mutating-family name was not rejected: {denied}")
        calls_after_invalid = read(env, name, "aave.mcp.get_fixture_call_count", {})
        invalid_after_count = calls_after_invalid.get("result", {}).get("structuredContent", {}).get("data", {}).get("tool_calls")
        require(invalid_after_count == invalid_before_count + 1,
                f"invalid arguments or mutating tool names reached MCP tools/call: before={invalid_before_count} after={invalid_after_count}")

        requests_before_invalid_bootstrap = read(env, name, "aave.mcp.get_fixture_request_count", {})
        before_request_count = requests_before_invalid_bootstrap.get("result", {}).get("structuredContent", {}).get("data", {}).get("requests")
        invalid_bootstrap = read(env, name, "aave.mcp.tools", {"unexpected": True}, expected=1)
        require(invalid_bootstrap.get("error", {}).get("code") == "AAVE_MCP_READ_INPUT_INVALID",
                f"invalid catalog arguments were not rejected before MCP dispatch: {invalid_bootstrap}")
        requests_after_invalid_bootstrap = read(env, name, "aave.mcp.get_fixture_request_count", {})
        after_request_count = requests_after_invalid_bootstrap.get("result", {}).get("structuredContent", {}).get("data", {}).get("requests")
        require(after_request_count == before_request_count + 4,
                f"invalid catalog arguments reached MCP HTTP transport: before={before_request_count} after={after_request_count}")

        if fixture == "aave-mcp-read-tool-error":
            failed = read(env, name, "aave.mcp.get_user_positions", {"user": USER}, expected=1)
            error = failed.get("error", {})
            require(error.get("code") == "AAVE_MCP_READ_FAILED",
                    f"upstream MCP isError was not surfaced as a failed read: {failed}")
            details = error.get("details", {})
            upstream = details.get("upstream_result", {})
            require(upstream.get("isError") is True
                    and upstream.get("structuredContent", {}).get("data", {}).get("partial") == "preserved"
                    and upstream.get("_meta", {}).get("warning"),
                    f"full untrusted upstream error result was not preserved: {failed}")
            return

        positions = read(env, name, "aave.mcp.get_user_positions", {"user": USER})
        require(positions.get("provenance") == "unverified_official_mcp", positions)
        data = positions.get("result", {}).get("structuredContent", {}).get("data", {})
        require(data.get("user") == USER and data.get("positions"),
                f"position response did not preserve MCP data: {positions}")
        require(data.get("coverage", {}).get("complete") is False
                and data.get("coverage", {}).get("nextCursor") == "synthetic-page-2",
                f"partial coverage/cursor was dropped: {positions}")
        require(data.get("warnings"), f"upstream warning was dropped: {positions}")

        new_metric = read(env, name, "aave.mcp.get_new_fixture_metric", {"user": USER})
        metric = new_metric.get("result", {}).get("structuredContent", {}).get("data", {})
        require(metric.get("metric") == "synthetic-only",
                f"newly discovered tool was not callable by its exact dynamic name: {new_metric}")

        operations_before_rewards = cli(env, name, "operations", "--json")
        rewards = read(env, name, "aave.mcp.get_user_rewards", {"user": USER})
        reward_data = rewards.get("result", {}).get("structuredContent", {}).get("data", {})
        tx = reward_data.get("rewards", [{}])[0].get("claimTransaction", {})
        require(tx.get("to") == "0x1111111111111111111111111111111111111111"
                and tx.get("data") == "0xdeadbeef",
                f"unsigned upstream transaction payload was not preserved as inert read data: {rewards}")
        require(rewards.get("provenance") == "unverified_official_mcp",
                f"rewards transaction was not labeled unverified: {rewards}")
        operations_after_rewards = cli(env, name, "operations", "--json")
        require(operations_after_rewards == operations_before_rewards,
                f"reading an upstream transaction payload created an operation: {operations_after_rewards}")

        calls_before_write_attempts = read(env, name, "aave.mcp.get_fixture_call_count", {})
        before_count = calls_before_write_attempts.get("result", {}).get("structuredContent", {}).get("data", {}).get("tool_calls")
        require(isinstance(before_count, int), f"fixture call counter is missing: {calls_before_write_attempts}")

        # A dynamically discovered read is never accepted as an executable write.
        operations_before = cli(env, name, "operations", "--json")
        execute = cli(
            env, name, "execute", "aave.mcp.get_user_positions",
            "--input", json.dumps({"user": USER}), "--id", "aave-mcp-read-must-not-execute", "--json",
            expected=1,
        )
        require(execute.get("error", {}).get("code") == "UNSUPPORTED_OPERATION",
                f"read tool was not rejected before financial execution: {execute}")
        plan = {
            "version": 1,
            "name": "Aave MCP read must not become a plan write",
            "steps": [{
                "step_id": "read-only",
                "operation": "aave.mcp.get_user_positions",
                "input": {"user": USER},
                "permitted_variations": {
                    "mode": "exact", "maximum_debits": [], "maximum_costs": [], "possible_credits": [],
                },
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            }],
            "aggregate_bounds": {"maximum_debits": [], "maximum_costs": []},
        }
        plan_result = cli(
            env, name, "plans", "preview", "--input", json.dumps(plan), "--json", expected=1,
        )
        require(plan_result.get("error", {}).get("code") == "UNSUPPORTED_OPERATION",
                f"dynamic read was invoked or accepted as a financial plan: {plan_result}")
        operations_after = cli(env, name, "operations", "--json")
        require(operations_after == operations_before,
                f"rejected dynamic read attempts created financial records: {operations_after}")
        calls_after_write_attempts = read(env, name, "aave.mcp.get_fixture_call_count", {})
        after_count = calls_after_write_attempts.get("result", {}).get("structuredContent", {}).get("data", {}).get("tool_calls")
        require(after_count == before_count + 1,
                f"execute/plan rejection contacted MCP tools/call: before={before_count} after={after_count}")
    finally:
        stop(env, name)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="gofin-aave-mcp-read-") as state:
        env = dict(os.environ)
        env["GOFIN_DEV_STATE_HOME"] = state
        env.pop("GOFIN_DEV_ENV", None)
        suffix = uuid.uuid4().hex[:10]
        run_fixture(env, f"aave-read-{suffix}", "aave-mcp-read")
        run_fixture(env, f"aave-down-{suffix}", "aave-mcp-read-unavailable")
        run_fixture(env, f"aave-error-{suffix}", "aave-mcp-read-tool-error")
        run_fixture(env, f"aave-stale-{suffix}", "aave-mcp-read-stale-catalog")
    print("Aave MCP public CLI read rehearsal passed (synthetic fixture, no RPC or credentials).")


if __name__ == "__main__":
    main()
