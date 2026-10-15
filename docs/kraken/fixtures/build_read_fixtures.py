#!/usr/bin/env python3
"""Build source-request fixtures without importing GoFin contracts or DTO schemas.

The static reviewed endpoint plan selects the public native endpoint; all option
names, required inputs, examples, enum branches and query placements come from
the independently pinned official OpenAPI snapshots. These fixtures establish
request mapping, not live response behavior or account eligibility.
"""
import copy
import hashlib
import json
import re
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
EXCLUDED = {"nonce", "otp", "new"}
ARTIFACTS = {"private_post_retrieve_export", "history_get_accountlogcsv"}


def resolve(spec, schema):
    schema = copy.deepcopy(schema)
    if "$ref" in schema:
        target = spec
        for segment in schema.pop("$ref").split("/")[1:]:
            target = target[segment.replace("~1", "/").replace("~0", "~")]
        schema = {**resolve(spec, target), **schema}
    return schema


def flatten(spec, schema):
    schema = resolve(spec, schema)
    for branch in schema.pop("allOf", []):
        branch = flatten(spec, branch)
        schema.setdefault("properties", {}).update(branch.get("properties", {}))
        schema.setdefault("required", []).extend(branch.get("required", []))
        schema.update({k: v for k, v in branch.items() if k not in ("properties", "required")})
    return schema


def sample(spec, schema, name="", example=None):
    schema = flatten(spec, schema)
    if example is not None:
        return example
    if "example" in schema:
        return schema["example"]
    if "enum" in schema:
        return next(v for v in schema["enum"] if v is not None)
    for union in ["oneOf", "anyOf"]:
        if union in schema:
            return sample(spec, schema[union][0], name)
    kind = schema.get("type", "object" if "properties" in schema else "string")
    if isinstance(kind, list):
        kind = next(t for t in kind if t != "null")
    if kind == "object":
        keys = schema.get("required", []) or list(schema.get("properties", {}))[:1]
        return {key: sample(spec, schema["properties"][key], key)
                for key in keys if key not in EXCLUDED}
    if kind == "array":
        return [sample(spec, schema.get("items", {}), name)] * max(1, schema.get("minItems", 1))
    if kind == "boolean":
        return False
    if kind in ("integer", "number"):
        value=schema.get("minimum", schema.get("exclusiveMinimum", 0) + 1)
        if schema.get("exclusiveMinimum") is True and "minimum" in schema:
            value+=1
        return str(value) if kind == "number" else value
    if schema.get("format") == "uuid" or "0-9a-fA-F" in schema.get("pattern", ""):
        return "00000000-0000-0000-0000-000000000001"
    if schema.get("format", "").lower() in ("date-time", "iso 8601") or name in ("from_ts", "to_ts", "lastFillTime", "lastTransferTime", "lastTime"):
        return "2026-10-10T00:00:00.000Z"
    pattern = schema.get("pattern", "")
    if "[0-9]+" in pattern or name in ("amount", "since"):
        return "0.000000000000000001" if name == "amount" else "1700000000"
    if name in ("asset", "asset_name"):
        return "ETH"
    if name in ("pair", "symbol", "tradeable"):
        return "PI_XBTUSD" if name != "pair" else "XETHZUSD"
    if name in ("id", "txid", "order_id", "cursor", "withdrawal_fee_token"):
        return "SYNTHETIC-ID-ONLY"
    if name == "accountUid":
        return "00000000-0000-0000-0000-000000000001"
    value = "SYNTHETIC"
    value += "X" * max(0, schema.get("minLength", 0) - len(value))
    return value[:schema.get("maxLength", len(value))]


def variants(spec, schema, name, example=None):
    schema = flatten(spec, schema)
    result = [("value", sample(spec, schema, name, example))]
    if "enum" in schema:
        result += [("enum:" + str(v), v) for v in schema["enum"] if v is not None]
    if schema.get("type") == "boolean":
        result += [("true", True), ("false", False)]
    if schema.get("type") == "array":
        item = flatten(spec, schema.get("items", {}))
        if "enum" in item:
            result += [("item:" + str(v), [v]) for v in item["enum"]]
    if schema.get("type") == "object" or "properties" in schema:
        baseline = sample(spec, schema, name)
        for field, child_schema in schema.get("properties", {}).items():
            for label, value in variants(spec, child_schema, field):
                result.append((f"field:{field}:{label}", {**baseline, field:value}))
    if schema.get("type") == "integer" and "enum" not in schema:
        for boundary in ["minimum", "maximum"]:
            if boundary in schema:
                value=schema[boundary]
                if schema.get("exclusive"+boundary.title()) is True:
                    value+=1 if boundary=="minimum" else -1
                result.append((boundary, value))
        if schema.get("minimum", 0) <= 0 and schema.get("exclusiveMinimum") is not True:
            result.append(("zero", 0))
    for union in ["oneOf", "anyOf"]:
        for i, branch in enumerate(schema.get(union, [])):
            for label, value in variants(spec, branch, name):
                result.append((f"{union}:{i}:{label}", value))
    if schema.get("nullable") or isinstance(schema.get("type"), list) and "null" in schema["type"]:
        result.append(("null", None))
    seen = set()
    return [(label, value) for label, value in result
            if not (json.dumps(value, sort_keys=True) in seen or seen.add(json.dumps(value, sort_keys=True)))]


def requests(spec, endpoint):
    path = spec["paths"][endpoint["path"]]
    operation = path[endpoint["verb"].lower()]
    properties, required, examples, query = {}, set(), {}, set()
    for parameter in path.get("parameters", []) + operation.get("parameters", []):
        parameter = resolve(spec, parameter)
        if parameter.get("in") not in ("query", "path") or parameter["name"] in EXCLUDED:
            continue
        name = parameter["name"]
        properties[name] = resolve(spec, parameter["schema"])
        if parameter.get("required"):
            required.add(name)
        if "example" in parameter:
            examples[name] = parameter["example"]
        if parameter["in"] == "query":
            query.add(name)
    body = resolve(spec, operation.get("requestBody", {})).get("content", {})
    for content in body.values():
        schema = flatten(spec, content.get("schema", {}))
        properties.update({k: v for k, v in schema.get("properties", {}).items() if k not in EXCLUDED})
        required.update(set(schema.get("required", [])) - EXCLUDED)
        if isinstance(content.get("example"), dict):
            examples.update(content["example"])
        break
    baseline = {name: sample(spec, properties[name], name, examples.get(name)) for name in sorted(required)}
    cases = [{"case": "required", "native": baseline}]
    for name, schema in properties.items():
        for label, value in variants(spec, schema, name, examples.get(name)):
            cases.append({"case": name + ":" + label, "option": name,
                          "native": {**baseline, name: value}})
    return operation, properties, sorted(query), cases


def response_examples(spec, endpoint):
    def escaped(value):
        return value.replace("~", "~0").replace("/", "~1")
    def dereference(value, pointer):
        while "$ref" in value:
            pointer=value["$ref"][1:]
            value=spec
            for key in pointer.split("/")[1:]:
                value=value[key.replace("~1", "/").replace("~0", "~")]
        return value,pointer
    pointer=f"/paths/{escaped(endpoint['path'])}/{endpoint['verb'].lower()}/responses/200"
    response=spec["paths"][endpoint["path"]][endpoint["verb"].lower()]["responses"].get("200",{})
    response,pointer=dereference(response,pointer)
    content=response.get("content",{}).get("application/json",{})
    pointer+="/content/application~1json"
    examples=[]
    def add(label,value,pointer):
        business_failure=isinstance(value,dict) and (value.get("result")=="error" or bool(value.get("error")))
        examples.append({"label":label,"pointer":pointer,"businessFailure":business_failure})
    if "example" in content:
        add("content-example",content["example"],pointer+"/example")
    for label,example in content.get("examples",{}).items():
        example,example_pointer=dereference(example,pointer+"/examples/"+escaped(label))
        if "value" in example:
            add(label,example["value"],example_pointer+"/value")
        elif any(key in example for key in ("result","error","serverTime")):
            # Some official Futures components store the response directly
            # rather than the OpenAPI ExampleObject's `value` wrapper.
            add(label,example,example_pointer)
    schema=content.get("schema",{})
    if "$ref" in schema and "example" in schema:
        add("schema-sibling-example",schema["example"],pointer+"/schema/example")
    schema,schema_pointer=dereference(schema,pointer+"/schema")
    if "example" in schema:
        add("schema-example",schema["example"],schema_pointer+"/example")
    return examples


def main():
    plan = json.loads((HERE / "read-endpoint-plan.json").read_text())["endpoints"]
    out = {"schemaVersion": 1, "provenance": "Independent pinned raw OpenAPI requests; no generated GoFin schema inputs",
           "numberPolicy": "OpenAPI number inputs use GoFin exact decimal text; integer, boolean and nullable inputs retain native units. No binary floating point conversion.",
           "sourceInterpretations": ["OpenAPI nullable:true permits explicit null in GoFin, including enums whose published value list omits null; actual venue null behavior remains read-only live evidence pending.", "Funding oneOf object scopes omit required discriminator keys/additionalProperties; meaningful method/network/group and status variants use the native documented discriminator.", "Legacy native string/integer/number unions overlap; GoFin accepts equivalent exact text through normalized anyOf without inventing numeric precision."],
           "endpoints": [], "excludedInputs": sorted(EXCLUDED), "limitations": []}
    for endpoint in plan:
        source = ROOT / (endpoint["sourceSpec"] + ".json")
        spec = json.loads(source.read_text())
        entry = {**endpoint, "sourceSha256": hashlib.sha256(source.read_bytes()).hexdigest()}
        if endpoint["path"] not in spec["paths"]:
            entry.update({"provenance": "Pinned CCXT only; current official schema unavailable",
                          "options": [], "query": [], "cases": [{"case": "required", "native": {}}]})
            out["limitations"].append({"method": endpoint["method"], "reason": entry["provenance"]})
        else:
            operation, properties, query, cases = requests(spec, endpoint)
            entry.update({"operationId": operation.get("operationId"), "options": sorted(properties),
                          "query": query, "cases": cases,
                          "responseExamples":response_examples(spec,endpoint)})
        if endpoint["method"] in ARTIFACTS:
            entry["artifact"] = True
            out["limitations"].append({"method": endpoint["method"], "reason": "Request mapping checked; artifact transfer/output verified by separate administration suite"})
        out["endpoints"].append(entry)
    (HERE / "read-native-options.json").write_text(json.dumps(out, indent=2) + "\n")
    print(json.dumps({"endpoints": len(out["endpoints"]), "cases": sum(len(e["cases"]) for e in out["endpoints"]),
                      "options": sum(len(e["options"]) for e in out["endpoints"]), "limitations": out["limitations"]}))


if __name__ == "__main__":
    main()
