#!/usr/bin/env python3
"""Verify native Kamino lending against public clones on an owned local validator.

Public RPC use is strictly read-only. Every simulation and signed transaction is
sent to loopback, using disposable synthetic balances and a fresh local keypair.
Evidence and cloned snapshots remain ignored under .context/kamino-lending-validator.
"""
from __future__ import annotations
import argparse, base64, importlib.util, json, os, shutil, socket, subprocess, time, tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
SPEC=importlib.util.spec_from_file_location("validator_helpers",ROOT/"scripts/verify-meteora-validator.py")
H=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(H)
EVIDENCE=ROOT/".context/kamino-lending-validator"
MAINNET="https://api.mainnet-beta.solana.com"
VAULT="HDsayqAsDWy3QvANGqh2yNraqcD8Fnjgh73Mhb3WRS5E"
PROGRAM="KvauGMspG5k6rtzrqqn7WNn3oZdyKqLKwK2XWQ8FLjd"
KLEND="KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
FARMS="FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"
TOKEN="TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
ZERO="11111111111111111111111111111111"

def setup():
    EVIDENCE.mkdir(parents=True,exist_ok=True)
    accounts={}; maximum_slot=0
    def fetch(addresses):
        nonlocal maximum_slot
        missing=[a for a in dict.fromkeys(addresses) if a!=ZERO and a not in accounts]
        for start in range(0,len(missing),16):
            page=missing[start:start+16]
            result=H.rpc(MAINNET,"getMultipleAccounts",[page,{"encoding":"base64","commitment":"finalized"}]);maximum_slot=max(maximum_slot,int(result["context"]["slot"]))
            for address,account in zip(page,result["value"]):
                if account is not None: accounts[address]=account
    def raw(address): return H.account_data(accounts[address],address)
    def pub(data,offset): return H.account_pubkey(data,offset)
    fetch([VAULT]);v=raw(VAULT)
    if len(v)!=62552 or accounts[VAULT]["owner"]!=PROGRAM: raise RuntimeError("Public vault identity/layout changed")
    mint=pub(v,80);shares=pub(v,184);token_vault=pub(v,120);lookup=pub(v,58568);farm=pub(v,58600)
    global_config=H.derive_address(PROGRAM,"string:global_config")
    fetch([mint,shares,token_vault,lookup,farm,global_config])
    reserves=[pub(v,312+i*2160) for i in range(25) if pub(v,312+i*2160)!=ZERO]
    ctokens=[pub(v,344+i*2160) for i in range(25) if pub(v,312+i*2160)!=ZERO]
    fetch(reserves+ctokens)
    for address in reserves:
        data=raw(address)
        if len(data)!=8624 or accounts[address]["owner"]!=KLEND: raise RuntimeError("Public reserve layout changed")
        fetch([pub(data,32),pub(data,128),pub(data,160),pub(data,192),pub(data,2560),pub(data,2600)])
    if farm!=ZERO:
        data=raw(farm)
        if len(data)!=8336 or accounts[farm]["owner"]!=FARMS: raise RuntimeError("Public farm layout changed")
        fetch([pub(data,40),pub(data,7256),pub(data,7536)])
    keypath=EVIDENCE/"synthetic-wallet.json"
    if keypath.exists():
        wallet=subprocess.check_output(["solana-keygen","pubkey",str(keypath)],text=True).strip()
    else: wallet=H.synthetic_keypair(keypath)
    ata=H.derive_address(H.ATA,f"pubkey:{wallet}",f"pubkey:{TOKEN}",f"pubkey:{mint}")
    decimals=raw(mint)[44];amount=1000*10**decimals
    synthetic={wallet:{"lamports":1_000_000_000_000,"data":["","base64"],"owner":ZERO,"executable":False,"rentEpoch":0},ata:{"lamports":2039280,"data":[base64.b64encode(H.synthetic_token_account(mint,wallet,amount,False,2039280)).decode(),"base64"],"owner":TOKEN,"executable":False,"rentEpoch":0}}
    files=[]
    for index,(address,account) in enumerate({**accounts,**synthetic}.items()):
        path=EVIDENCE/f"account-{index}.json";H.write_genesis_account(path,address,account);files.append([address,str(path)])
    manifest={"slot":maximum_slot,"vault":VAULT,"mint":mint,"decimals":decimals,"shares":shares,"farm":farm,"wallet":wallet,"wallet_key":str(keypath),"accounts":files,"synthetic_balance_atoms":str(amount),"public_read_only":MAINNET}
    (EVIDENCE/"manifest.json").write_text(json.dumps(manifest,indent=2)+"\n")
    return manifest

def lock_semantics_fixture(manifest):
    """Synthetic farm/user state solely for deployed-program maturity semantics."""
    manifest=dict(manifest); accounts={address:json.loads(Path(path).read_text())["account"] for address,path in manifest["accounts"]}
    farm=manifest["farm"];wallet=manifest["wallet"];mint=manifest["shares"];slot=manifest["slot"];wad=10**18
    data=bytearray(H.account_data(accounts[farm],farm));vault=H.account_pubkey(data,7256)
    data[7360]=1
    data[7400:7404]=(1000).to_bytes(4,"little")
    data[7424:7440]=(10*wad).to_bytes(16,"little");data[7440:7448]=(10).to_bytes(8,"little")
    data[7496:7504]=(1).to_bytes(8,"little");data[7512:7520]=(1000).to_bytes(8,"little")
    accounts[farm]["data"]=[base64.b64encode(data).decode(),"base64"]
    vault_data=bytearray(H.account_data(accounts[vault],vault));vault_data[64:72]=(int.from_bytes(vault_data[64:72],"little")+10).to_bytes(8,"little");accounts[vault]["data"]=[base64.b64encode(vault_data).decode(),"base64"]
    user=H.derive_address(FARMS,"string:user",f"pubkey:{farm}",f"pubkey:{wallet}")
    def key_bytes(value):
        number=0
        for char in value: number=number*58+"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".index(char)
        return number.to_bytes(32,"big")
    user_data=bytearray(920);user_data[:8]=bytes([72,177,85,249,76,167,186,126]);user_data[16:48]=key_bytes(farm);user_data[48:80]=key_bytes(wallet);user_data[480:512]=key_bytes(wallet)
    user_data[408:424]=(wad).to_bytes(16,"little");user_data[424:440]=(10*wad).to_bytes(16,"little");user_data[440:448]=(slot+200).to_bytes(8,"little");user_data[512:520]=(slot-10000).to_bytes(8,"little")
    accounts[user]={"owner":FARMS,"lamports":10000000,"data":[base64.b64encode(user_data).decode(),"base64"],"executable":False,"rentEpoch":0}
    ata=H.derive_address(H.ATA,f"pubkey:{wallet}",f"pubkey:{TOKEN}",f"pubkey:{mint}")
    accounts[ata]={"owner":TOKEN,"lamports":2039280,"data":[base64.b64encode(H.synthetic_token_account(mint,wallet,100,False,2039280)).decode(),"base64"],"executable":False,"rentEpoch":0}
    files=[]
    for index,(address,account) in enumerate(accounts.items()):
        path=EVIDENCE/f"lock-semantics-account-{index}.json";H.write_genesis_account(path,address,account);files.append([address,str(path)])
    manifest["accounts"]=files;manifest["synthetic_lock_semantics_fixture"]=True
    path=EVIDENCE/"lock-semantics-manifest.json";path.write_text(json.dumps(manifest,indent=2)+"\n")
    return manifest,path

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument("--setup",action="store_true");parser.add_argument("--port",type=int,default=19321);parser.add_argument("--farm-lock-semantics",action="store_true");args=parser.parse_args()
    if args.setup: setup();print("Saved public clones and synthetic local genesis in",EVIDENCE);return 0
    manifest_path=EVIDENCE/"manifest.json";manifest=json.loads(manifest_path.read_text())
    if args.farm_lock_semantics: manifest,manifest_path=lock_semantics_fixture(manifest)
    url=f"http://127.0.0.1:{args.port}"
    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1",args.port))==0:raise RuntimeError("Requested loopback port is occupied")
    evidence_prefix="lock-semantics-" if args.farm_lock_semantics else ""
    ledger=Path(tempfile.mkdtemp(prefix="gofin-kamino-",dir="/tmp"))
    cmd=["solana-test-validator","--ledger",str(ledger),"--url",MAINNET,"--rpc-port",str(args.port),"--faucet-port",str(args.port+10000),"--gossip-port",str(args.port+10001),"--dynamic-port-range",f"{args.port+10002}-{args.port+10102}","--warp-slot",str(manifest["slot"]+100),"--clone-upgradeable-program",PROGRAM,KLEND,FARMS,TOKEN,"--clone",H.ATA]
    for address,path in manifest["accounts"]:cmd.extend(["--account",address,path])
    (EVIDENCE/f"{evidence_prefix}command.json").write_text(json.dumps({"command":cmd,"submission_endpoint":url},indent=2)+"\n")
    with (EVIDENCE/f"{evidence_prefix}validator.log").open("w") as log:
        process=subprocess.Popen(cmd,cwd=ROOT,stdout=log,stderr=subprocess.STDOUT)
        try:
            deadline=time.monotonic()+180
            while time.monotonic()<deadline:
                if process.poll() is not None:raise RuntimeError("Owned validator exited; inspect .context/kamino-lending-validator/validator.log")
                try:
                    if H.rpc(url,"getHealth",[])=="ok":break
                except Exception:pass
                time.sleep(1)
            else:raise RuntimeError("Owned validator did not become healthy")
            env={**os.environ,"GOFIN_KAMINO_LENDING_VALIDATOR_RPC":url,"GOFIN_KAMINO_LENDING_VALIDATOR_MANIFEST":str(manifest_path),"GOFIN_KAMINO_LENDING_VALIDATOR_EVIDENCE":str(EVIDENCE/"execution.json")}
            with (EVIDENCE/f"{evidence_prefix}tests.log").open("w") as test_log:
                result=subprocess.run(["cargo","test","-p","gofin-kamino","--test","lending_validator",("farm_stake_resets_existing_continuous_lock_and_pending_warmup" if args.farm_lock_semantics else "deposit_stake_partial_and_full_native_withdrawal"),"--","--ignored","--nocapture","--test-threads=1"],cwd=ROOT,env=env,stdout=test_log,stderr=subprocess.STDOUT)
            print((EVIDENCE/f"{evidence_prefix}tests.log").read_text());return result.returncode
        finally:
            process.terminate()
            try:process.wait(timeout=20)
            except subprocess.TimeoutExpired:process.kill();process.wait()
            shutil.rmtree(ledger)
if __name__=="__main__":raise SystemExit(main())
