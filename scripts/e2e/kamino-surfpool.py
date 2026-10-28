#!/usr/bin/env python3
"""Run a synthetic Kamino broker fixture on an isolated patched Surfpool."""
import argparse, base64, importlib.util, json, os, re, socket, subprocess, sys, tempfile, time
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
PROGRAMS = {
 'borrowing':['KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD','FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr'],
 'lending':['KvauGMspG5k6rtzrqqn7WNn3oZdyKqLKwK2XWQ8FLjd','KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD','FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr'],
 'liquidity':['6LtLpnUFNByNXLyCoK9wA2MykKAmQNZKBdY8s47dehDc','whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc','CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK','LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo','FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr']}
COMMON=['TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA','TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb','ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL','MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr']
def main():
 p=argparse.ArgumentParser();p.add_argument('family',choices=PROGRAMS);p.add_argument('--cli',required=True);p.add_argument('--summary',required=True);args=p.parse_args()
 directory=ROOT/'.context'/f'kamino-{args.family}-validator'; manifest=directory/'manifest.json'
 if not manifest.exists(): raise RuntimeError(f'Missing synthetic fixture {manifest}; run its public-read-only preparation first')
 spec=importlib.util.spec_from_file_location('meteora_cli_validator',ROOT/'scripts/test-meteora-cli-validator.py')
 helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper);helper.validate_surfpool()
 m=json.loads(manifest.read_text()); entries=m['accounts']; snapshot={x:None for x in PROGRAMS[args.family]+COMMON}
 if args.family == 'borrowing':
  second=directory/'synthetic-second-wallet.json'
  if not second.exists():
   subprocess.run(['solana-keygen','new','--no-bip39-passphrase','--silent','--outfile',str(second)],check=True,capture_output=True)
  second_address=subprocess.check_output(['solana-keygen','pubkey',str(second)],text=True).strip()
  snapshot[second_address]={'lamports':1000000000,'owner':'11111111111111111111111111111111','executable':False,'rentEpoch':0,'data':'','parsedData':None}
  spec=importlib.util.spec_from_file_location('solana_fixture',ROOT/'scripts/verify-meteora-validator.py')
  fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
  mint=m['reserves'][0]['mint'];token=COMMON[0]
  ata=fixture.derive_address(fixture.ATA,f'pubkey:{second_address}',f'pubkey:{token}',f'pubkey:{mint}')
  data=fixture.synthetic_token_account(mint,second_address,0,False,2039280)
  snapshot[ata]={'lamports':2039280,'owner':token,'executable':False,'rentEpoch':0,'data':base64.b64encode(data).decode(),'parsedData':None}
 for row in entries:
  address,path=(row if isinstance(row,list) else (row,str(directory/'accounts'/f'{row}.json')))
  account=json.loads(Path(path).read_text())['account']; data=account['data']
  snapshot[address]={'lamports':account['lamports'],'owner':account['owner'],'executable':account['executable'],'rentEpoch':account['rentEpoch'],'data':data[0] if isinstance(data,list) else data,'parsedData':None}
 with tempfile.TemporaryDirectory(prefix='gofin-kamino-surfpool-') as tmp:
  snap=Path(tmp)/'snapshot.json';snap.write_text(json.dumps(snapshot))
  with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
  url=f'http://127.0.0.1:{port}'; log=directory/'surfpool.log'
  with log.open('w') as out:
   proc=subprocess.Popen(['surfpool','start','--no-tui','--no-studio','--no-deploy','--host','127.0.0.1','--rpc-url','https://api.mainnet-beta.solana.com','--port',str(port),'--ws-port',str(port+1),'--airdrop-amount','0','--snapshot',str(snap)],cwd=ROOT,stdout=out,stderr=subprocess.STDOUT)
   try:
    import urllib.request
    deadline=time.monotonic()+240
    while time.monotonic()<deadline:
     if proc.poll() is not None: raise RuntimeError(f'Surfpool exited; inspect {log}')
     try:
      request=urllib.request.Request(url,data=json.dumps({'jsonrpc':'2.0','id':1,'method':'getBlockHeight','params':[{'commitment':'finalized'}]}).encode(),headers={'Content-Type':'application/json'})
      if json.load(urllib.request.urlopen(request,timeout=2)).get('result',0)>0:break
     except Exception:time.sleep(1)
    else:raise RuntimeError(f'Surfpool not ready; inspect {log}')
    env={**os.environ,'GOFIN_KAMINO_CLI':args.cli,'GOFIN_KAMINO_POSITIVE_RPC':url,'GOFIN_KAMINO_POSITIVE_MANIFEST':str(manifest),'GOFIN_SURFPOOL_ADVANCE':'1'}
    if args.family == 'borrowing': env['GOFIN_KAMINO_SECOND_KEY']=str(second)
    name=f'positive_kamino_{args.family}_via_public_cli'
    result=subprocess.run(['cargo','test','--locked','-p','gofin-broker','--test','kamino_positive_validator',name,'--','--ignored','--exact','--nocapture'],cwd=ROOT,env=env,text=True,capture_output=True)
    print(result.stdout,end='');print(result.stderr,end='',file=sys.stderr)
    matches=re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',result.stdout)
    passed=result.returncode==0 and matches==[('1','0','0')]
    Path(args.summary).write_text(json.dumps({'scenario':f'kamino-{args.family}','passed':passed,'selected_tests':1 if passed else 0,'environment':'synthetic_surfpool','public_rpc':'read_only','submission_rpc':'loopback'},indent=2)+'\n')
    return 0 if passed else 1
   finally:
    proc.terminate()
    try:proc.wait(timeout=15)
    except subprocess.TimeoutExpired:proc.kill();proc.wait()
if __name__=='__main__':sys.exit(main())
