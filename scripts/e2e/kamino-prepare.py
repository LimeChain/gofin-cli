#!/usr/bin/env python3
"""Prepare synthetic Kamino snapshots using public, read-only Solana RPC."""
import argparse, base64, importlib.util, json, subprocess
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
p=argparse.ArgumentParser();p.add_argument('family',choices=['borrowing','lending','liquidity']);args=p.parse_args()
path=ROOT/'scripts'/f'verify-kamino-{args.family}-validator.py'
spec=importlib.util.spec_from_file_location('kamino_fixture',path);module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
if args.family=='liquidity':
    manifest=module.prepare(['5BUR22MZR9YivcYpsx9xp2aDJGKBcF9HWCYynBPw6r8'],False)
    # Seed the synthetic owner's empty share ATA. It is a genuine SPL account,
    # avoiding a public lookup for an account that exists only in this fork.
    strategy=manifest['strategies'][0]['address']
    row=json.loads((module.OUT/'accounts'/f'{strategy}.json').read_text())['account']
    state=module.decode(module.load_idl('liquidity'),'WhirlpoolStrategy',base64.b64decode(row['data'][0]))
    mint=state['sharesMint']; wallet=manifest['wallet']
    ata=module.derive(module.ATA,f'pubkey:{wallet}',f'pubkey:{module.TOKEN}',f'pubkey:{mint}')
    (module.OUT/'accounts'/f'{ata}.json').write_text(json.dumps({'pubkey':ata,'account':module.token_account(mint,wallet,0,module.TOKEN)}))
    manifest['accounts'].append(ata)
    manifest['synthetic_policy']['precreated_empty_share_ata']=ata
    (module.OUT/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
elif args.family=='lending':
    module.setup()
else:
    manifest=module.setup()
    # Refresh all public accounts in one RPC context. Reserve accounting and
    # its token vault must describe the same slot; sequential snapshots can
    # trigger an on-chain arithmetic failure even with correct instructions.
    wallet=manifest['wallet']; synthetic={wallet}
    for reserve in manifest['reserves']:
        synthetic.add(module.H.derive_address(module.H.ATA,f'pubkey:{wallet}',f'pubkey:{module.TOKEN}',f'pubkey:{reserve["mint"]}'))
    paths=dict(manifest['accounts']); addresses=[a for a in paths if a not in synthetic]
    if len(addresses)>100: raise RuntimeError('Borrowing snapshot exceeds atomic RPC account limit')
    response=module.H.rpc(module.MAINNET,'getMultipleAccounts',[addresses,{'encoding':'base64','commitment':'finalized'}])
    accounts=dict(zip(addresses,response['value']))
    if any(value is None for value in accounts.values()): raise RuntimeError('Atomic borrowing snapshot lost a required account')
    raw=module.EVIDENCE/'atomic-public-reserves.json'
    raw.write_text(json.dumps([{'address':r['address'],'account':accounts[r['address']]} for r in manifest['reserves']]))
    deps=json.loads(subprocess.check_output(['cargo','run','--quiet','-p','gofin-kamino','--example','klend_snapshot_dependencies','--',str(raw)],cwd=ROOT,text=True))
    if deps['reserves']!=manifest['reserves'] or not set(deps['dependencies']).issubset(accounts):
        raise RuntimeError('Borrowing reserve identities or dependencies changed during atomic snapshot')
    for reserve in deps['reserves']:
        account=accounts[reserve['address']]; data=bytearray(module.H.account_data(account,reserve['address']))
        for offset in deps['oracle_age_offsets']:data[offset:offset+8]=(86400).to_bytes(8,'little')
        account['data']=[base64.b64encode(data).decode(),'base64']
    for address,account in accounts.items(): module.H.write_genesis_account(Path(paths[address]),address,account)
    manifest['slot']=int(response['context']['slot']);manifest['public_accounts_atomic_slot']=manifest['slot']
    (module.EVIDENCE/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
