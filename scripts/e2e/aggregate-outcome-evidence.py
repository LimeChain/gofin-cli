#!/usr/bin/env python3
"""Combine observed per-venue counters for one required acceptance scenario."""
import argparse
import json
from pathlib import Path


def aggregate(scenario, paths):
    components = [json.loads(path.read_text()) for path in paths]
    assert components, "At least one observed component is required"
    assert len({component["scenario"] for component in components}) == len(components), "Duplicate component evidence"
    result = {"scenario": scenario, "components": components}
    for key in ("approval_count", "external_submission_count", "probe_approval_count", "probe_submission_count", "intervention_count"):
        values = [component.get(key, 0) if key.startswith("probe_") else component[key] for component in components]
        assert all(type(value) is int and value >= 0 for value in values), key
        result[key] = sum(values)
    for key in ("remaining_external_uncertainty", "blocked_unrelated_activity"):
        values = [component[key] for component in components]
        assert all(type(value) is bool for value in values), key
        result[key] = any(values)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario")
    parser.add_argument("output", type=Path)
    parser.add_argument("components", nargs="+", type=Path)
    args = parser.parse_args()
    args.output.write_text(json.dumps(aggregate(args.scenario, args.components), indent=2) + "\n")
