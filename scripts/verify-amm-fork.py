#!/usr/bin/env python3
"""Explicit AMM forks. Own each node; never load credentials or attach to another node."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import urllib.request
from fin_dev_resources import allocate_loopback, start_owned_process

SOURCE = Path(__file__).resolve().parent.parent
URLS = {1: "https://ethereum-rpc.publicnode.com", 42161: "https://arb1.arbitrum.io/rpc", 4663: "https://rpc.mainnet.chain.robinhood.com"}
# Exact asset fixtures, independent of product code and discovery defaults.
TOKENS = {1: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", 42161: "0xaf88d065e77c8cc2239327c5edb3a432268e5831", 4663: "0x5fc5360d0400a0fd4f2af552add042d716f1d168"}

def rpc(url, method, params):
    if url.startswith("https://"):
        # Foundry uses the same supported HTTPS transport as the owned Anvil.
        result = subprocess.run([str(Path.home()/".foundry/bin/cast"), "rpc", method, *[json.dumps(p) for p in params], "--rpc-url", url], capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise RuntimeError(f"{method}: public archive read failed: {result.stderr[:160]}")
        return json.loads(result.stdout)
    request = urllib.request.Request(url, data=json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode(), headers={"Content-Type":"application/json"})
    with urllib.request.urlopen(request, timeout=15) as response:
        value = json.load(response)
    if "error" in value:
        raise RuntimeError(f"{method}: {value['error']}")
    return value["result"]

def protocol_config(chain):
    text = (SOURCE / "crates/fin-broker/src/amm_deployments.rs").read_text()
    start = text.index(f"{chain} => UniswapConfig")
    fragment = text[start:text.index("        },", start)+10]
    contracts = dict(re.findall(r'(factory|quoter|router|pool_manager|state_view): "(0x[0-9a-f]+)"', fragment.split("v4: Some")[0]))
    v4 = dict(re.findall(r'(quoter|router|pool_manager|state_view): "(0x[0-9a-f]+)"', fragment.split("v4: Some")[1]))
    return {**contracts,"router_kind":"swap_router02","v4":{**v4,"router_has_hop_price":True}}

def run(chain, evidence, legacy_v3=False):
    head = rpc(URLS[chain], "eth_getBlockByNumber", ["latest", False])
    # Avoid public-provider replicas disagreeing at the tip. Record one exact
    # mature block/hash and require that immutable anchor at the owned node.
    depth = 32 if chain == 1 else 128
    anchor = rpc(URLS[chain], "eth_getBlockByNumber", [hex(int(head["number"],16)-depth), False])
    fixture = {"chain_id":chain,"block":int(anchor["number"],16),"hash":anchor["hash"],"token":TOKENS[chain],"uniswap":protocol_config(chain),"v4_pool_key":{"currency0":"0x"+"0"*40,"currency1":TOKENS[chain],"fee":500,"tick_spacing":10,"hooks":"0x"+"0"*40}}
    if legacy_v3:
        if chain != 1:
            raise RuntimeError("Legacy router regression is reviewed on chain 1 only")
        fixture["uniswap"]["router_kind"] = "legacy_v3"
        fixture["uniswap"]["router"] = "0xe592427a0aece92de3edee1f18e0157c05861564"
    (evidence/"fixture.json").write_text(json.dumps(fixture,indent=2)+"\n")
    lease = allocate_loopback()
    endpoint, port = lease.url, lease.port
    lease.release()
    node = start_owned_process([str(Path.home()/".foundry/bin/anvil"),"--host","127.0.0.1","--port",str(port),"--chain-id",str(chain),"--fork-url",URLS[chain],"--fork-block-number",str(fixture["block"]),"--timestamp",str(int(time.time())),"--base-fee","100000000","--gas-price","10000000","--disable-min-priority-fee","--no-cors"],log_path=evidence/"anvil.log",marker_path=evidence/"anvil.owner.json",env=os.environ.copy())
    test = None
    try:
        metadata = None
        for _ in range(60):
            if node.process.poll() is not None:
                break
            try:
                metadata=rpc(endpoint,"anvil_metadata",[])
                if metadata.get("chainId")==chain:
                    break
            except (OSError,RuntimeError):
                pass
            time.sleep(.25)
        if not metadata or metadata.get("chainId")!=chain:
            raise RuntimeError("Owned Anvil/archive did not become ready")
        if metadata["forkedNetwork"]["forkBlockHash"]!=fixture["hash"] or metadata["forkedNetwork"]["forkBlockNumber"]!=fixture["block"]:
            raise RuntimeError("Owned fork anchor mismatch")
        env=os.environ.copy()
        env.update(FIN_AMM_FORK_URL=endpoint,FIN_AMM_FORK_INSTANCE=metadata["instanceId"],FIN_AMM_FORK_FIXTURE=json.dumps(fixture))
        test=start_owned_process(["cargo","test","--locked","--manifest-path",str(SOURCE/"Cargo.toml"),"-p","fin-broker","--test","amm_roundtrip","--","--ignored","--nocapture"],log_path=evidence/"test.log",marker_path=evidence/"test.owner.json",env=env)
        return test.process.wait()
    finally:
        if test is not None and not test.stop():
            test.detach()
            node.detach()
            raise RuntimeError("Owned test cleanup unresolved; node retained for reconciliation")
        if not node.stop():
            node.detach()
            raise RuntimeError("Owned node cleanup unresolved")

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chain",type=int,choices=list(URLS))
    parser.add_argument("--legacy-v3",action="store_true",help="Verify preserved Ethereum legacy router encoding")
    args=parser.parse_args()
    if args.legacy_v3 and args.chain != 1:
        parser.error("--legacy-v3 requires --chain 1")
    results=[]
    for chain in ([args.chain] if args.chain else URLS):
        evidence=Path(tempfile.mkdtemp(prefix=f"amm-{'legacy-' if args.legacy_v3 else ''}fork-{chain}.",dir=SOURCE/"work"))
        try:
            status=run(chain,evidence,args.legacy_v3)
            row={"chain_id":chain,"legacy_v3":args.legacy_v3,"exit":status,"result":"passed" if status==0 else "failed","evidence":str(evidence),"production_writes":0}
        except (OSError,RuntimeError,KeyError,ValueError) as error:
            row={"chain_id":chain,"result":"blocked","reason":str(error),"evidence":str(evidence),"production_writes":0}
        results.append(row)
        print(json.dumps(row),flush=True)
    return 0 if all(r["result"]=="passed" for r in results) else 1

if __name__=="__main__":
    raise SystemExit(main())
