#!/usr/bin/env python3
"""Read public Kamino state and run native lifecycle tests on an owned validator.

No production keys or service are read. Public RPC only supplies account/program
clones. A fresh synthetic wallet and token balances exist exclusively in local
genesis, and the Rust sender refuses non-loopback URLs.
"""
from __future__ import annotations
import argparse, base64, hashlib, json, os, shutil, socket, subprocess, tempfile, time, urllib.request
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
OUT=ROOT/'.context'/'kamino-liquidity-validator'
RPC='https://api.mainnet-beta.solana.com'
PROGRAM='6LtLpnUFNByNXLyCoK9wA2MykKAmQNZKBdY8s47dehDc'
DEX=['whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc','CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK','LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo']
FARMS='FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr'
TOKEN='TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
TOKEN2022='TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb'
ZERO='11111111111111111111111111111111'
ATA='ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
MEMO='MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr'
# Public, on-chain observed LIVE strategies, one per native liquidity backend.
DEFAULT_STRATEGIES=['5BUR22MZR9YivcYpsx9xp2aDJGKBcF9HWCYynBPw6r8','3Efia5dDK2dyWyCc92VF2ep9dTBH74cY8bt2LUciq3qS','Aj7o7RrGtpmHDKUvqeDpU33CXpvSbwBfEd7emZUr8DYY']
def rpc(method,params,url=RPC):
 request=urllib.request.Request(url,data=json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params}).encode(),headers={'Content-Type':'application/json'})
 with urllib.request.urlopen(request,timeout=30) as response:value=json.load(response)
 if 'error'in value:raise RuntimeError(f'{method}: {value["error"]}')
 return value['result']
def b58(raw):
 alphabet='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';n=int.from_bytes(raw,'big');result=''
 while n:n,r=divmod(n,58);result=alphabet[r]+result
 return '1'*(len(raw)-len(raw.lstrip(b'\0')))+result
def un58(s):
 n=0
 for c in s:n=n*58+'123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'.index(c)
 return b'\0'*(len(s)-len(s.lstrip('1')))+(n.to_bytes((n.bit_length()+7)//8,'big') if n else b'')
def derive(program,*seeds):
 r=subprocess.run(['solana','find-program-derived-address','--no-address-labels','--output','json',program,*seeds],text=True,capture_output=True,check=True)
 value=json.loads(r.stdout)
 return value.get('address') or value.get('pubkey')
def load_idl(name):return json.loads((ROOT/'crates/gofin-kamino/resources'/f'{name}-idl.json').read_text())
def decode(idl,name,data):
 types={d['name']:d['type'] for d in idl['types']};account=next(d for d in idl['accounts'] if d['name']==name);offset=8
 if data[:8]!=hashlib.sha256(f'account:{name}'.encode()).digest()[:8]:raise ValueError(f'Unexpected {name} discriminator')
 def read(t):
  nonlocal offset
  if isinstance(t,str):
   if t=='publicKey':value=b58(data[offset:offset+32]);offset+=32;return value
   size=1 if t=='bool'else int(t[1:])//8;value=int.from_bytes(data[offset:offset+size],'little',signed=t.startswith('i'));offset+=size;return value
  if 'array'in t:return [read(t['array'][0]) for _ in range(t['array'][1])]
  return fields(types[t['defined']])
 def fields(t):return {f['name']:read(f['type']) for f in t['fields']}
 result=fields(account['type'])
 if offset!=len(data):raise ValueError(f'Unexpected {name} layout size {len(data)} expected {offset}')
 return result
def patch_u64(idl,name,data,updates):
 types={d['name']:d['type'] for d in idl['types']}
 def size(t):
  if isinstance(t,str):return 32 if t=='publicKey'else 1 if t=='bool'else int(t[1:])//8
  if 'array'in t:return size(t['array'][0])*t['array'][1]
  return sum(size(f['type'])for f in types[t['defined']]['fields'])
 result=bytearray(data);offset=8
 for field in next(a for a in idl['accounts']if a['name']==name)['type']['fields']:
  if field['name']in updates:
   if field['type']!='u64':raise ValueError('Synthetic policy patch expects u64')
   result[offset:offset+8]=updates[field['name']].to_bytes(8,'little')
  offset+=size(field['type'])
 return bytes(result)
def token_account(mint,owner,amount,program):
 data=bytearray(165);data[:32]=un58(mint);data[32:64]=un58(owner);data[64:72]=amount.to_bytes(8,'little');data[108]=1
 return {'lamports':2039280,'data':[base64.b64encode(data).decode(),'base64'],'owner':program,'executable':False,'rentEpoch':0}
def prepare(strategies,stake=False):
 OUT.mkdir(parents=True,exist_ok=True);store={}
 def fetch(keys):
  keys=list(dict.fromkeys(k for k in keys if k and k!=ZERO and k not in store))
  for offset in range(0,len(keys),50):
   batch=keys[offset:offset+50];result=rpc('getMultipleAccounts',[batch,{'encoding':'base64','commitment':'finalized'}])
   for key,account in zip(batch,result['value']):
    if account is not None:store[key]=account
   time.sleep(.12)
 def account(key):fetch([key]);return base64.b64decode(store[key]['data'][0])
 fixture_rows=[];idl=load_idl('liquidity')
 for address in strategies:
  s=decode(idl,'WhirlpoolStrategy',account(address));g=decode(idl,'GlobalConfig',account(s['globalConfig']));coll=decode(idl,'CollateralInfos',account(g['tokenInfos']))['infos']
  # Static oracle/pool clones cannot refresh each other. Bound their local-only
  # price guard explicitly; record the synthetic policy change in the manifest.
  store[address]['data'][0]=base64.b64encode(patch_u64(idl,'WhirlpoolStrategy',account(address),{'maxPriceDeviationBps':10000,'depositMaxPoolPriceOracleDeviationBps':10000})).decode()
  store[s['globalConfig']]['data'][0]=base64.b64encode(patch_u64(idl,'GlobalConfig',account(s['globalConfig']),{'depositMaxPoolPriceOracleDeviationBps':10000})).decode()
  relevant=['tokenAVault','tokenBVault','baseVaultAuthority','pool','position','positionMint','positionTokenAccount','poolTokenVaultA','poolTokenVaultB','tokenAMint','tokenBMint','sharesMint','sharesMintAuthority','tickArrayLower','tickArrayUpper','strategyLookupTable','raydiumProtocolPositionOrBaseVaultAuthority','raydiumPoolConfigOrBaseVaultAuthority','farm','reward0Vault','reward1Vault','reward2Vault']
  keys=[s[k]for k in relevant];keys.extend([g['tokenInfos'],g['scopeProgramId']]);keys.extend(coll[s[k]]['scopeFeed']for k in ['tokenACollateralId','tokenBCollateralId'])
  for i in range(3):
   if s[f'reward{i}Vault'] not in [ZERO,s['baseVaultAuthority']]:keys.append(coll[s[f'reward{i}CollateralId']]['scopeFeed'])
  for reward in s['kaminoRewards']:
   if reward['rewardMint']!=ZERO and reward['decimals']>0:keys.extend([reward['rewardMint'],coll[reward['rewardCollateralId']]['scopeFeed']])
  for mint in [s['tokenAMint'],s['tokenBMint']]:keys.append(derive(PROGRAM,'string:treasury_fee_vault',f'pubkey:{mint}'))
  keys.append(derive(PROGRAM,'string:treasury_fee_vault_authority'))
  if s['strategyDex']==1:keys.append(derive(DEX[1],'string:pool_tick_array_bitmap_extension',f'pubkey:{s["pool"]}'))
  if s['strategyDex']==2:keys.append(derive(DEX[2],'string:__event_authority'))
  fetch(keys)
  dex=s['strategyDex'];pool=decode(load_idl('meteora')if dex==2 else idl,['Whirlpool','PoolState','LbPair'][dex],account(s['pool']))
  rewards=pool['rewardInfos'];fetch([r.get('tokenVault',r.get('vault',ZERO))for r in rewards]+[r.get('tokenMint',r.get('mint',ZERO))for r in rewards])
  if s['farm']!=ZERO:
   farm=account(s['farm'])
   if len(farm)!=8336 or store[s['farm']]['owner']!=FARMS:raise ValueError('Unexpected farm layout')
   fetch([b58(farm[offset:offset+32]) for offset in [40,7256,7536]])
  if stake and s['farm']==ZERO:raise ValueError('Requested staked lifecycle requires a configured farm')
  fixture_rows.append({'address':address,'backend':['orca','raydium','meteora'][dex],'assets':[{'mint':s['tokenAMint'],'decimals':s['tokenAMintDecimals'],'token_program':s['tokenATokenProgram']if s['tokenATokenProgram']!=ZERO else TOKEN},{'mint':s['tokenBMint'],'decimals':s['tokenBMintDecimals'],'token_program':s['tokenBTokenProgram']if s['tokenBTokenProgram']!=ZERO else TOKEN}]})
 keyfile=OUT/'synthetic-wallet.json'
 if not keyfile.exists():subprocess.run(['solana-keygen','new','--no-bip39-passphrase','--silent','--outfile',str(keyfile)],check=True,capture_output=True)
 wallet=subprocess.run(['solana-keygen','pubkey',str(keyfile)],capture_output=True,text=True,check=True).stdout.strip()
 store[wallet]={'lamports':100000000000,'data':['','base64'],'owner':ZERO,'executable':False,'rentEpoch':0}
 for row in fixture_rows:
  for asset in row['assets']:
   ata=derive(ATA,f'pubkey:{wallet}',f'pubkey:{asset["token_program"]}',f'pubkey:{asset["mint"]}')
   store[ata]=token_account(asset['mint'],wallet,1000*10**asset['decimals'],asset['token_program'])
 account_dir=OUT/'accounts';account_dir.mkdir(exist_ok=True)
 for key,value in store.items():
  if value.get('executable'):continue
  (account_dir/f'{key}.json').write_text(json.dumps({'pubkey':key,'account':value}))
 manifest={'deposit_units':500 if stake else 1,'stake':stake,'public_rpc':RPC,'wallet':wallet,'wallet_key':str(keyfile),'strategies':fixture_rows,'accounts':[k for k,v in store.items()if not v.get('executable')],'synthetic_policy':{'maxPriceDeviationBps':10000,'depositMaxPoolPriceOracleDeviationBps':10000},'clone_slot':rpc('getSlot',[{'commitment':'finalized'}]),'evidence_kind':'public account clones with synthetic local token balances'}
 (OUT/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n');return manifest

def main():
 parser=argparse.ArgumentParser();parser.add_argument('--prepare-only',action='store_true');parser.add_argument('--stake',action='store_true');parser.add_argument('--reuse',action='store_true');parser.add_argument('--strategy',action='append');parser.add_argument('--port',type=int,default=19099);args=parser.parse_args()
 for tool in ['solana','solana-keygen','solana-test-validator','cargo']:
  if not shutil.which(tool):raise SystemExit(f'Missing {tool}')
 manifest=json.loads((OUT/'manifest.json').read_text())if args.reuse else prepare(args.strategy or DEFAULT_STRATEGIES,args.stake)
 if args.prepare_only:print(f'Prepared ignored cloned-state fixture: {OUT}');return
 with socket.socket()as s:
  if s.connect_ex(('127.0.0.1',args.port))==0:raise SystemExit('Validator port already in use; choose another --port')
 ledger=Path(tempfile.mkdtemp(prefix='gofin-kl-',dir='/tmp'))
 cmd=['solana-test-validator','--reset','--quiet','--warp-slot',str(manifest.get('clone_slot') or rpc('getSlot',[{'commitment':'finalized'}])),'--ledger',str(ledger),'--rpc-port',str(args.port),'--gossip-port',str(args.port+2),'--faucet-port',str(args.port+3),'--dynamic-port-range',f'{args.port+10}-{args.port+30}','--bind-address','127.0.0.1','--url',RPC,'--clone-upgradeable-program',PROGRAM,*DEX,TOKEN,TOKEN2022,FARMS,'--clone',MEMO,ATA]
 for key in manifest['accounts']:cmd+=['--account',key,str(OUT/'accounts'/f'{key}.json')]
 logfile=(OUT/'validator.log').open('w');process=subprocess.Popen(cmd,stdout=logfile,stderr=subprocess.STDOUT)
 url=f'http://127.0.0.1:{args.port}'
 try:
  for _ in range(120):
   if process.poll()is not None:raise RuntimeError('Validator exited; inspect ignored validator.log')
   try:
    if rpc('getHealth',[],url)=='ok':break
   except Exception:pass
   time.sleep(.5)
  else:raise RuntimeError('Validator did not become ready')
  env=os.environ.copy();env.update({'GOFIN_KAMINO_LIQUIDITY_VALIDATOR_RPC':url,'GOFIN_KAMINO_LIQUIDITY_VALIDATOR_MANIFEST':str(OUT/'manifest.json')})
  result=subprocess.Popen(['cargo','test','--locked','-p','gofin-kamino','--test','liquidity_validator','--','--ignored','--nocapture'],cwd=ROOT,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
  with (OUT/'execution.log').open('w')as execution_log:
   for line in result.stdout:
    execution_log.write(line);execution_log.flush();print(line,end='',flush=True)
  code=result.wait()
  (OUT/'result.json').write_text(json.dumps({'exit_code':code,'kind':manifest['evidence_kind'],'manifest':str(OUT/'manifest.json'),'execution_log':str(OUT/'execution.log')},indent=2)+'\n')
  raise SystemExit(code)
 finally:process.terminate();process.wait(timeout=20);logfile.close();shutil.rmtree(ledger)
if __name__=='__main__':main()
