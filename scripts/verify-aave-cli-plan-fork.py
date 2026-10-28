#!/usr/bin/env python3
"""Aave lifecycle through public CLI, one named plan, owned local fork only.

Reuses the owned AMM transport/bootstrap helpers. All financial writes use GoFin.
Synthetic funding/mining and canonical pin RPCs are recorded orchestration gaps.
Never reads installed credentials or the existing live-development environment.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid
from gofin_dev_resources import allocate_loopback, start_owned_process
from aave_fork_receipt_proxy import ReceiptProxy

SOURCE = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location('amm_cli_rehearsal', Path(__file__).with_name('verify-amm-cli-plan-fork.py'))
AMM = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AMM)
SECONDARY_KEY = '0' * 63 + '1'  # Public synthetic fixture; never fund outside the owned fork.
WETH = {1:'0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2',8453:'0x4200000000000000000000000000000000000006',42161:'0x82af49447d8a07e3bd95bd0d56f35241523fbab1'}
POOL = {1:'0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2',8453:'0xa238dd80c259a72e81d7e4664a9801593f98d1c5',42161:'0x794a61358d6845594f94dc1db02a252b5b4814ad'}


class AaveRehearsal(AMM.Rehearsal):
    receipt_proxy = None
    saved_pin = None
    rpc_raw = staticmethod(AMM.FORK.rpc)
    def close(self):
        try: super().close()
        finally:
            if self.receipt_proxy: self.receipt_proxy.stop()

    def refresh_owned_fork_head(self):
        # The fork begins at a historical canonical block. Advance only the
        # owned Anvil head so GoFin's production 60-second RPC freshness check
        # sees a current timestamp; never relax or replace that check.
        header=self.rpc('eth_getBlockByNumber',['latest',False],'Inspect owned fork head freshness before CLI dispatch')
        timestamp=int(header['timestamp'],16);now=int(time.time())
        if timestamp>now+30:
            raise RuntimeError('Owned fork latest block is future-dated beyond GoFin freshness tolerance')
        if now-timestamp>30:
            self.rpc('evm_setNextBlockTimestamp',[now],'Advance owned historical fork to current wall clock')
            self.rpc('evm_mine',[],'Mine current-timestamp owned fork block for production RPC freshness validation')
            current=self.rpc('eth_getBlockByNumber',['latest',False],'Verify freshly timestamped owned fork head')
            if abs(int(current['timestamp'],16)-int(time.time()))>30:
                raise RuntimeError('Owned fork did not advance to a fresh timestamp')

    def cli(self,*argv,error=None):
        self.refresh_owned_fork_head()
        return super().cli(*argv,error=error)

    def settled(self, operation, payload, plan=None, step=None):
        argv=['execute',operation,'--input',json.dumps(payload),'--id',payload['request_id']]
        if plan: argv += ['--plan',plan,'--step',step]
        try: record=self.cli(*argv)
        except AssertionError as failure:
            rejected=failure.args[0]
            assert isinstance(rejected,dict) and rejected.get('error',{}).get('code')=='EVIDENCE_TIMEOUT', rejected
            attempts=[r for r in self.operations() if r['request_id']==payload['request_id']]
            assert len(attempts)<=1 and all(r['status']=='failed' and not r.get('submission') and not r.get('dispatch_evidence') for r in attempts)
            assert rejected['error']['stage']=='preflight'
            assert not any(r.get('input',{}).get('request_id')==payload['request_id'] and r.get('submission') for r in self.operations())
            # A confirmed unsubmitted timeout consumes no write authority. The
            # same grant/input may make ONE fresh attempt; never renew consent.
            self.save('unsubmitted-timeout',{'original_id':attempts[0]['id'] if attempts else None,'new_attempt_id':payload['request_id']+'-fresh-evidence','scope_unchanged':True,'submission_absent':True})
            # Allow a bounded provider read cooldown before a fresh candidate.
            # This neither changes the grant nor extends its expiry.
            time.sleep(45)
            argv[argv.index('--id')+1]=payload['request_id']+'-fresh-evidence'
            record=self.cli(*argv)
        for _ in range(40):
            if record['status'] in ('completed','failed','cancelled','rejected'):break
            self.rpc('anvil_mine',['0x2'],'Owned local settlement advancement; no GoFin block-mining command')
            record=self.cli('status',record['id'])
        assert record['status']=='completed', record
        duplicate=self.cli(*argv)
        assert duplicate['id']==record['id'] and duplicate['submission']==record['submission'] and duplicate['effects']==record['effects']
        return record

    def start(self):
        chain = AMM.CHAIN
        pin_env = {**os.environ,'GOFIN_AAVE_PIN_CHAIN_ID':str(chain)}
        pin_env.pop('GOFIN_AAVE_PIN_BLOCK',None)
        if self.saved_pin:
            assert self.saved_pin['chain_id']==chain
            pin_env['GOFIN_AAVE_PIN_BLOCK']=str(self.saved_pin['block'])
        result = subprocess.run(['cargo','test','--locked','-p','gofin-ethereum','--test','aave_fork_pin','--','--ignored','--exact','public_aave_fork_pin','--nocapture'],cwd=SOURCE,env=pin_env,capture_output=True,text=True,timeout=600)
        (self.directory/'pin.log').write_text(result.stdout+result.stderr)
        assert result.returncode==0, 'Public Rust transport pin failed; no fork writes ran'
        pins=[json.loads(line.removeprefix('AAVE_FORK_PIN=')) for line in result.stdout.splitlines() if line.startswith('AAVE_FORK_PIN=')]
        assert len(pins)==1 and pins[0]['chain_id']==chain
        pin=pins[0]
        if self.saved_pin: assert pin['hash']==self.saved_pin['hash'], 'Saved canonical block changed before any fork write'
        self.save('archive-anchor',pin)
        lease=allocate_loopback();self.endpoint,port=lease.url,lease.port;lease.release()
        self.node=start_owned_process([str(Path.home()/'.foundry/bin/anvil'),'--host','127.0.0.1','--port',str(port),'--chain-id',str(chain),*(['--optimism'] if chain==8453 else []),'--fork-url',AMM.FORK.URLS[chain],'--fork-block-number',str(pin['block']),'--timestamp',str(int(time.time())),'--base-fee','100000000','--gas-price','10000000','--disable-min-priority-fee','--no-cors','--block-time','1'],log_path=self.directory/'anvil.log',marker_path=self.directory/'anvil.owner.json',env=self.env)
        assert self.node.process.args[-2:]==['--block-time','1'], 'Owned Anvil must mine against wall time during long RPC workflows'
        for _ in range(80):
            try:self.identity=AMM.FORK.rpc(self.endpoint,'anvil_metadata',[]);break
            except (OSError,RuntimeError):time.sleep(.25)
        assert self.identity and self.identity['chainId']==chain
        assert self.identity['forkedNetwork']['forkBlockHash']==pin['hash']
        keyfile=self.directory/'PUBLIC-SYNTHETIC-NEVER-FUND.env';keyfile.write_text(f'GOFIN_DEV_EVM_PRIVATE_KEY={AMM.KEY}\nGOFIN_DEV_EVM_SECONDARY_PRIVATE_KEY={SECONDARY_KEY}\n');keyfile.chmod(0o600)
        self.make('dev-env-import',DEV_CREDENTIALS_FILE=str(keyfile),DEV_FLAGS='--json');keyfile.unlink()
        worktree_key=hashlib.sha256(str(SOURCE).encode()).hexdigest()[:20]
        self.config_path=Path(self.env['GOFIN_DEV_STATE_HOME'])/worktree_key/self.name/'config.json'
        self.receipt_proxy=ReceiptProxy(self,self.endpoint,chain,WETH[chain]);self.endpoint=self.receipt_proxy.start()
        config=json.loads(self.config_path.read_text());assert config['environment_id']==hashlib.sha256(f'{worktree_key}:{self.name}'.encode()).hexdigest()
        self.owner,self.secondary=config['evm_addresses'][:2];config['amm_fork_prices']=True;config['rpc_urls']={str(chain):[self.endpoint]};config['reviewed_chain_ids']=[chain]
        self.config_path.write_text(json.dumps(config,indent=2)+'\n');self.config_path.chmod(0o600)
        self.rpc('anvil_setBalance',[self.owner,hex(3*10**16)],'Synthetic local native fixture funding; GoFin does not mint balances')
        self.rpc('anvil_setBalance',[self.secondary,hex(10**16)],'Synthetic secondary-wallet gas funding on owned fork')
        self.started=True;self.make('dev-env-start',DEV_MODE='live',DEV_FLAGS='--json')
        self.cli('--help');self.cli('capabilities')
        for op in ['plans.authorize','aave.supply','aave.borrow','aave.repay','aave.withdraw','aave.collateral']:
            self.cli('schema',op)
        # Pre-fund repayment interest through GoFin before the financial baseline.
        seed,_,_=self.swap('uniswap','native',AMM.TOKEN,'0.00005',500)
        self.settled('uniswap.swap',seed)

    def state(self):
        return {'balances':self.read('wallet.balances',{**AMM.BASE,'tokens':[WETH[AMM.CHAIN],AMM.TOKEN]}),'positions':self.read('aave.positions',{**AMM.BASE,'tokens':[WETH[AMM.CHAIN],AMM.TOKEN]})}

    def plan(self):
        chain=AMM.CHAIN;weth=WETH[chain];usdc=AMM.TOKEN;pool=POOL[chain];resource=self.initial['balances']['resource']
        def asset(ref):return {'network':f'ethereum:{chain}','reference':ref,'decimals':6 if ref==usdc else 18}
        def amount(ref,n):return {'asset':asset(ref),'atoms':str(n)}
        def debit(ref,n):return {'resource':resource,'amount':amount(ref,n)}
        def cost(ref,n,position=False):
            loc={'kind':'position','resource':resource,'position':f'aave:{pool}:{weth}:supply','position_kind':'supply'} if position else {'kind':'cash','resource':resource}
            return {'location':loc,'amount':amount(ref,n),'treatment':'additional_debit' if ref=='native' else 'included_in_debit'}
        steps=[]
        def add(name,op,extra,bounds=None,debits=None,credits=None,deps=None):
            inp={**AMM.BASE,'request_id':self.name+'-'+name,'max_fee_eth':'0.001',**extra}
            variation=bounds if bounds is not None else {'mode':'exact','maximum_debits':debits or [],'maximum_costs':[cost('native',1000000000000000)],'possible_credits':[{'resource':resource,'asset':asset(ref)} for ref in credits or []]}
            steps.append({'step_id':name,'operation':op,'input':inp,'permitted_variations':variation,'completion':{'operation_succeeded':True},'funding_dependencies':deps or []})
        def bounds(lo,hi,**extra):return {'min_amount':lo,'max_amount':hi,'max_fee_eth':'0.001',**extra}
        add('wrap','wallet.wrap',{'amount':'0.0012'},debits=[debit('native',1200000000000000)],credits=[weth])
        add('approve-weth','wallet.approve',{'token':weth,'spender':pool,'amount':'0.0012'})
        add('supply','aave.supply',{'token':weth,'market':pool,'amount':'0.0012'},bounds('0.0012','0.0012'),deps=[{'producing_step_id':'wrap','resource':resource,'asset':asset(weth)}])
        add('collateral-off','aave.collateral',{'token':weth,'market':pool,'enabled':False},bounds('0','0'))
        add('collateral-on','aave.collateral',{'token':weth,'market':pool,'enabled':True},bounds('0','0'))
        add('borrow','aave.borrow',{'token':usdc,'market':pool,'amount':'0.5','min_health_factor':'3','max_debt_usd':'0.6','max_debt_to_collateral_bps':2000},bounds('0.45','0.5',max_cumulative_borrow='0.50001'))
        add('approve-usdc','wallet.approve',{'token':usdc,'spender':pool,'amount':'0.5001'})
        add('partial-repay','aave.repay',{'token':usdc,'market':pool,'amount':'0.25'},bounds('0.25','0.25'))
        add('full-repay','aave.repay',{'token':usdc,'market':pool,'amount':'all','max_amount':'0.2501'},bounds('0','0.2501'))
        add('partial-withdraw','aave.withdraw',{'token':weth,'market':pool,'amount':'0.0004','recipient':self.owner},bounds('0.0004','0.0004'))
        add('full-withdraw','aave.withdraw',{'token':weth,'market':pool,'amount':'all','max_amount':'0.000801','recipient':self.owner},bounds('0','0.000801'))
        add('restore-weth','wallet.approve',{'token':weth,'spender':pool,'amount':'0'})
        add('restore-usdc','wallet.approve',{'token':usdc,'spender':pool,'amount':'0'})
        add('unwrap','wallet.unwrap',{'amount':'all','max_amount':'0.001201'},debits=[debit(weth,1201000000000000)],credits=['native'])
        return {'version':1,'name':'Owned Aave public CLI lifecycle and restoration','steps':steps,'aggregate_bounds':{'maximum_debits':[debit('native',1200000000000000),debit(weth,2401000000000000),debit(usdc,500100)],'maximum_costs':[cost('native',14000000000000000),cost(weth,1200000000000000),cost(usdc,500100),cost(weth,1201000000000000,True)]}}

    def run(self, positive_only=False):
        self.initial=self.state();positions=self.initial['positions']['data'];assert int(positions['account']['debt_base'])==0
        assert all(int(r[k])==0 for r in positions['positions'] for k in ['supplied','stable_debt','variable_debt','pool_allowance'])
        plan=self.plan();review=self.cli('plans','preview','--input',json.dumps(plan),'--valid-for','30m')
        grant=self.cli('plans','authorize','--input',json.dumps(plan),'--id',self.name+'-grant','--review-hash',review['review_hash'],'--valid-for','30m');pid=grant['plan_id']
        before=self.submissions()
        if not positive_only:
            last=plan['steps'][-1]
            self.cli('execute',last['operation'],'--input',json.dumps(last['input']),'--id',self.name+'-out-of-order','--plan',pid,'--step',last['step_id'],'--dry-run',error=True)
            assert self.submissions()==before
            changed={**plan['steps'][0]['input'],'amount':'0.0013'}
            self.cli('execute','wallet.wrap','--input',json.dumps(changed),'--id',self.name+'-out-of-bounds','--plan',pid,'--step','wrap','--dry-run',error=True)
            assert self.submissions()==before
        records=[];secondary_fee=0
        for step in plan['steps']:
            if step['step_id']=='supply' and not positive_only:
                # The development-only MCP fixture returns an ApprovalRequired
                # batch for this exact one-wei request, with a valid supply
                # hidden in originalTransaction. Exercise public CLI rejection
                # after WETH funding/allowance exists and before the real supply.
                bad={**step['input'],'request_id':self.name+'-mcp-approval-plan','amount':'0.000000000000000001'}
                before_reject=self.submissions()
                rejected=self.cli('execute','aave.supply','--input',json.dumps(bad),'--id',bad['request_id'])
                assert rejected['status']=='rejected' and rejected['error']['code']=='AAVE_MCP_REJECTED', rejected
                assert rejected.get('approval') is None and rejected.get('authorization_source') is None
                assert rejected.get('submission') is None and rejected.get('dispatch_evidence') is None
                assert self.submissions()==before_reject
                attempts=[r for r in self.operations() if r['request_id']==bad['request_id']]
                assert len(attempts)==1
                assert all(r.get('approval') is None and r.get('authorization_source') is None for r in attempts)
                assert all(r.get('submission') is None and r.get('dispatch_evidence') is None for r in attempts)
                self.save('mcp-approval-plan-rejected',{'request_id':bad['request_id'],'error':'AAVE_MCP_REJECTED','approval_audit_created':False,'submission_or_dispatch_created':False,'hidden_original_supply_present':True})
            r=self.settled(step['operation'],step['input'],pid,step['step_id']);records.append(r)
            if step['step_id']=='borrow':
                if positive_only:
                    def secondary_usdc():
                        observed=self.read('wallet.balances',{**AMM.BASE,'wallet':'dev-wallet-secondary','tokens':[AMM.TOKEN]})
                        return next(int(row['amount']['atoms']) for row in observed['data'] if row['amount']['asset']['reference']==AMM.TOKEN)
                    initial_secondary=secondary_usdc()
                    for source,recipient in [('dev-wallet',self.secondary),('dev-wallet-secondary',self.owner)]:
                        transfer={**AMM.BASE,'wallet':source,'request_id':self.name+'-transfer-'+source,'token':AMM.TOKEN,'amount':'0.1','recipient':recipient,'max_fee_eth':'0.001'}
                        moved=self.settled('wallet.transfer',transfer)
                        records.append(moved)
                        assert secondary_usdc()==initial_secondary+(100000 if source=='dev-wallet' else 0)
                        if source=='dev-wallet-secondary':
                            secondary_fee=sum(int(e['amount']['atoms']) for e in moved['effects']['effects'] if e['kind']=='cost' and e['amount']['asset']['reference']=='native')
                evidence=r['last_reconciliation_evidence'];risk=self.read('aave.positions',{**AMM.BASE,'tokens':[WETH[AMM.CHAIN],AMM.TOKEN],'block_number':evidence['block_number']});assert risk['block_hash']==evidence['block_hash'];a=risk['data']['account'];assert int(a['health_factor'])>=3*10**18 and int(a['debt_base'])*10000<=int(a['collateral_base'])*2000
                if not positive_only:
                    self.restart();recovered=self.cli('status',r['id']);assert recovered['submission']==r['submission'] and recovered['effects']==r['effects']
        final=self.state();pos=final['positions']['data'];assert int(pos['account']['debt_base'])==0
        assert all(int(r[k])==0 for r in pos['positions'] for k in ['supplied','stable_debt','variable_debt','scaled_supply','scaled_variable_debt','pool_allowance']);assert all(not r['collateral_enabled'] for r in pos['positions'])
        w=WETH[AMM.CHAIN];u=AMM.TOKEN
        balance=lambda state:{r['amount']['asset']['reference']:int(r['amount']['atoms']) for r in state['balances']['data']}
        b=balance(self.initial);f=balance(final);effects=[e for r in records for e in r['effects']['effects']]
        fees=sum(int(e['amount']['atoms']) for e in effects if e['kind']=='cost' and e['amount']['asset']['reference']=='native')
        wrapped=int(AMM.conversion(records[0])['spent']['atoms']);unwrapped=int(AMM.conversion(records[-1])['received']['atoms'])
        assert f[w]==b[w]==0 and f['native']-b['native']==unwrapped-wrapped-fees+secondary_fee
        usdc_cost=sum(int(e['amount']['atoms']) for e in effects if e['kind']=='cost' and e['amount']['asset']['reference']==u);assert b[u]-f[u]==usdc_cost
        status=self.cli('plans','status',pid);assert all(s['state']=='completed' for s in status['steps']) and not status['usage']['reservations']
        if not positive_only:
            self.cli('plans','cancel',pid);self.restart();revoked=self.cli('plans','status',pid);assert revoked['revision']['status']=='revoked' and revoked['usage']==status['usage']
        assert self.submissions()==before+14+(2 if positive_only else 0)
        self.save('result',{'chain_id':AMM.CHAIN,'public_cli_named_plan_lifecycle':'passed','financial_submissions':14+(2 if positive_only else 0),'production_writes':0,'scaled_closure_and_allowances':True,'cash_conservation_exact':True,'duplicates_preserved':14+(2 if positive_only else 0),'second_wallet_transfer_round_trip':positive_only,'restart_with_outstanding_debt':not positive_only,'revocation_persisted':not positive_only,'out_of_order_and_bound_rejections_without_submission':not positive_only,'mcp_approval_batch_rejected_before_submission':not positive_only,'approval':'isolated development automatic; no Touch ID','transport':'Owned Anvil deployed state; deterministic local Aave MCP response fixture; local rollup fee fixtures; synthetic fixed owned-fork pricing'})


    def run_horizon(self):
        """Use issuer-authorized synthetic USTB on the actual Horizon fork."""
        collateral='0x43415eb6ff9db7e26a15b704e7a3edce97d31c4e';usdc=AMM.TOKEN;pool=POOL[1]
        allowlist='0xbcbc2b4fb2abe1c598c9ea91e0b03338e4728d1f';cast=str(Path.home()/'.foundry/bin/cast')
        def command(*args):
            result=subprocess.run([cast,*map(str,args)],capture_output=True,text=True,timeout=30)
            if result.returncode:raise RuntimeError('Owned Horizon fixture cast failed: '+result.stderr[-500:])
            return result.stdout.strip()
        def call(contract,signature,*args):return self.rpc('eth_call',[{'to':contract,'data':command('calldata',signature,*args)},'latest'],'Read exact owned Horizon issuer fixture')
        def addr(contract,signature):return '0x'+call(contract,signature)[-40:]
        admin=addr(allowlist,'owner()');issuer=addr(collateral,'owner()')
        for account in {admin,issuer}:
            self.rpc('anvil_setBalance',[account,hex(10**18)],'Fund issuer admin gas on owned synthetic fork')
            self.rpc('anvil_impersonateAccount',[account],'Impersonate issuer authority only on exact owned fork')
        try:
            def send(sender,target,signature,*args):
                self.guard();command('send','--unlocked','--from',sender,'--rpc-url',self.endpoint,target,signature,*args)
            send(admin,allowlist,'setAllowed(address,address,bool)',self.owner,collateral,'true')
            send(issuer,collateral,'mint(address,uint256)',self.owner,'5000000')
        finally:
            for account in {admin,issuer}:self.rpc('anvil_stopImpersonatingAccount',[account],'End fork-only issuer authority impersonation')
        assert int(call(collateral,'balanceOf(address)',self.owner),16)==5000000
        self.save('horizon-fixture',{'pool':pool,'collateral':collateral,'synthetic_issuer_allowlist_and_mint':True,'production_writes':0})
        def execute(operation, **extra):return self.settled(operation,{**AMM.BASE,'request_id':str(uuid.uuid4()),'max_fee_eth':'0.001',**extra})
        def positions():return self.read('aave.positions',{**AMM.BASE,'market':pool,'tokens':[collateral,usdc]})['data']
        execute('wallet.approve',token=collateral,spender=pool,amount='5')
        execute('aave.supply',token=collateral,market=pool,amount='5')
        execute('aave.collateral',token=collateral,market=pool,enabled=False)
        execute('aave.collateral',token=collateral,market=pool,enabled=True)
        execute('aave.borrow',token=usdc,market=pool,amount='0.5',min_health_factor='3',max_debt_usd='0.6',max_debt_to_collateral_bps=2000)
        assert int(positions()['account']['debt_base'])>0
        def secondary():
            rows=self.read('wallet.balances',{**AMM.BASE,'wallet':'dev-wallet-secondary','tokens':[usdc]})['data']
            return next(int(row['amount']['atoms']) for row in rows if row['amount']['asset']['reference']==usdc)
        initial=secondary();execute('wallet.transfer',token=usdc,amount='0.1',recipient=self.secondary);assert secondary()==initial+100000
        self.settled('wallet.transfer',{**AMM.BASE,'wallet':'dev-wallet-secondary','request_id':str(uuid.uuid4()),'token':usdc,'amount':'0.1','recipient':self.owner,'max_fee_eth':'0.001'});assert secondary()==initial
        execute('wallet.approve',token=usdc,spender=pool,amount='0.5001')
        execute('aave.repay',token=usdc,market=pool,amount='all',max_amount='0.5001')
        execute('aave.withdraw',token=collateral,market=pool,amount='all',max_amount='5.01',recipient=self.owner)
        execute('wallet.approve',token=collateral,spender=pool,amount='0');execute('wallet.approve',token=usdc,spender=pool,amount='0')
        final=positions();assert int(final['account']['debt_base'])==0
        assert all(int(r[k])==0 for r in final['positions'] for k in ['supplied','stable_debt','variable_debt','scaled_supply','scaled_variable_debt','pool_allowance'])
        self.save('result',{'chain_id':1,'market':pool,'collateral':collateral,'lifecycle':'passed','synthetic_issuer_allowlist_and_mint':True,'production_writes':0})

    def run_etherfi(self):
        """Enable isolated fixture caps on the actual retiring EtherFi market."""
        collateral='0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee';usdc=AMM.TOKEN;pool=POOL[1]
        provider='0xeba440b438ad808101d1c451c1c5322c90befcda'
        cast=str(Path.home()/'.foundry/bin/cast')
        def command(*args):
            result=subprocess.run([cast,*map(str,args)],capture_output=True,text=True,timeout=30)
            if result.returncode: raise RuntimeError('Owned EtherFi fixture cast failed: '+result.stderr[-500:])
            return result.stdout.strip()
        def call(contract,signature,*args):
            return self.rpc('eth_call',[{'to':contract,'data':command('calldata',signature,*args)},'latest'],'Read exact owned EtherFi fixture configuration')
        def addr(signature):return '0x'+call(provider,signature)[-40:]
        admin=addr('getACLAdmin()');acl=addr('getACLManager()');configurator=addr('getPoolConfigurator()')
        self.rpc('anvil_setBalance',[admin,hex(10**18)],'Fund synthetic fork-only impersonated market administrator')
        self.rpc('anvil_impersonateAccount',[admin],'Impersonate reviewed market ACL administrator only on exact owned fork')
        try:
            def admin_send(target,signature,*args):
                self.guard();command('send','--unlocked','--from',admin,'--rpc-url',self.endpoint,target,signature,*args)
            admin_send(acl,'addRiskAdmin(address)',admin)
            admin_send(configurator,'setSupplyCap(address,uint256)',collateral,'1000000')
            admin_send(configurator,'setBorrowCap(address,uint256)',usdc,'1000000')
        finally:self.rpc('anvil_stopImpersonatingAccount',[admin],'End fork-only administrator impersonation')
        amount=10**16
        reserve=call(pool,'getReserveData(address)',collateral)
        holder='0x'+reserve[2+8*64+24:2+9*64]
        self.rpc('anvil_setBalance',[holder,hex(10**18)],'Fund synthetic fork-only reserve holder gas')
        self.rpc('anvil_impersonateAccount',[holder],'Impersonate owned-fork reserve holder for synthetic collateral funding')
        try:
            self.guard();command('send','--unlocked','--from',holder,'--rpc-url',self.endpoint,collateral,'transfer(address,uint256)',self.owner,str(amount))
        finally:self.rpc('anvil_stopImpersonatingAccount',[holder],'End fork-only reserve holder impersonation')
        assert int(call(collateral,'balanceOf(address)',self.owner),16)==amount
        self.save('etherfi-fixture',{'pool':pool,'admin':admin,'synthetic_cap_overrides':True,'synthetic_collateral_atoms':str(amount),'production_writes':0})
        def execute(operation, **extra):return self.settled(operation,{**AMM.BASE,'request_id':str(uuid.uuid4()),'max_fee_eth':'0.001',**extra})
        def positions():return self.read('aave.positions',{**AMM.BASE,'market':pool,'tokens':[collateral,usdc]})['data']
        execute('wallet.approve',token=collateral,spender=pool,amount='0.002')
        execute('aave.supply',token=collateral,market=pool,amount='0.002')
        execute('aave.collateral',token=collateral,market=pool,enabled=False)
        execute('aave.collateral',token=collateral,market=pool,enabled=True)
        execute('aave.borrow',token=usdc,market=pool,amount='0.5',min_health_factor='3',max_debt_usd='0.6',max_debt_to_collateral_bps=2000)
        assert int(positions()['account']['debt_base'])>0
        def secondary():
            rows=self.read('wallet.balances',{**AMM.BASE,'wallet':'dev-wallet-secondary','tokens':[usdc]})['data']
            return next(int(row['amount']['atoms']) for row in rows if row['amount']['asset']['reference']==usdc)
        initial_secondary=secondary()
        execute('wallet.transfer',token=usdc,amount='0.1',recipient=self.secondary)
        assert secondary()==initial_secondary+100000
        self.settled('wallet.transfer',{**AMM.BASE,'wallet':'dev-wallet-secondary','request_id':str(uuid.uuid4()),'token':usdc,'amount':'0.1','recipient':self.owner,'max_fee_eth':'0.001'})
        assert secondary()==initial_secondary
        execute('wallet.approve',token=usdc,spender=pool,amount='0.5001')
        execute('aave.repay',token=usdc,market=pool,amount='all',max_amount='0.5001')
        execute('aave.withdraw',token=collateral,market=pool,amount='all',max_amount='0.00201',recipient=self.owner)
        execute('wallet.approve',token=collateral,spender=pool,amount='0');execute('wallet.approve',token=usdc,spender=pool,amount='0')
        final=positions();assert int(final['account']['debt_base'])==0
        assert all(int(r[k])==0 for r in final['positions'] for k in ['supplied','stable_debt','variable_debt','scaled_supply','scaled_variable_debt','pool_allowance'])
        self.save('result',{'chain_id':1,'market':pool,'lifecycle':'passed','fixture_caps_overridden':True,'synthetic_collateral':True,'production_writes':0})

    def run_lido(self):
        """WETH supply and WETH debt preserve Lido's distinct reserve identity."""
        weth=WETH[1];pool=POOL[1]
        def positions():
            return self.read('aave.positions',{**AMM.BASE,'market':pool,'tokens':[weth]})['data']
        def execute(operation, **extra):
            return self.settled(operation,{**AMM.BASE,'request_id':str(uuid.uuid4()),'max_fee_eth':'0.001',**extra})
        initial=positions();assert int(initial['account']['debt_base'])==0
        execute('wallet.wrap',amount='0.003')
        execute('wallet.approve',token=weth,spender=pool,amount='0.003')
        execute('aave.supply',token=weth,market=pool,amount='0.002')
        execute('aave.collateral',token=weth,market=pool,enabled=False)
        execute('aave.collateral',token=weth,market=pool,enabled=True)
        execute('aave.borrow',token=weth,market=pool,amount='0.0001',min_health_factor='3',max_debt_usd='0.6',max_debt_to_collateral_bps=2000)
        held=positions();assert int(held['account']['debt_base'])>0 and int(held['account']['health_factor'])>=3*10**18
        def secondary():
            rows=self.read('wallet.balances',{**AMM.BASE,'wallet':'dev-wallet-secondary','tokens':[weth]})['data']
            return next(int(row['amount']['atoms']) for row in rows if row['amount']['asset']['reference']==weth)
        prior=secondary()
        execute('wallet.transfer',token=weth,amount='0.0001',recipient=self.secondary)
        assert secondary()==prior+10**14
        reverse={**AMM.BASE,'wallet':'dev-wallet-secondary','request_id':str(uuid.uuid4()),'token':weth,'amount':'0.0001','recipient':self.owner,'max_fee_eth':'0.001'}
        self.settled('wallet.transfer',reverse);assert secondary()==prior
        execute('wallet.approve',token=weth,spender=pool,amount='0.000101')
        execute('aave.repay',token=weth,market=pool,amount='all',max_amount='0.000101')
        execute('aave.withdraw',token=weth,market=pool,amount='all',max_amount='0.00201',recipient=self.owner)
        execute('wallet.approve',token=weth,spender=pool,amount='0')
        execute('wallet.unwrap',amount='all',max_amount='0.00301')
        final=positions();assert int(final['account']['debt_base'])==0
        assert all(int(r[k])==0 for r in final['positions'] for k in ['supplied','stable_debt','variable_debt','scaled_supply','scaled_variable_debt','pool_allowance'])
        self.save('result',{'chain_id':1,'market':pool,'lifecycle':'passed','second_wallet_transfer_round_trip':True,'production_writes':0,'valuation':'synthetic fixed owned-fork prices','approval':'isolated development automatic'})


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--chain',type=int,choices=[1,8453,42161],required=True);parser.add_argument('--pin',type=Path);parser.add_argument('--fork-rpc-url',help='Public archive RPC; owned fork must match the pinned canonical hash');parser.add_argument('--market',choices=['core','lido','etherfi','horizon'],default='core');parser.add_argument('--positive-only',action='store_true',help='Run the successful lifecycle without rejection cases');args=parser.parse_args()
    if args.market in ('lido','etherfi','horizon'):
        if args.chain!=1: parser.error('Selected market is Ethereum-only')
        POOL[1]={'lido':'0x4e033931ad43597d96d6bcc25c280717730b58b1','etherfi':'0x0aa97c284e98396202b6a04024f5e2c65026f3c0','horizon':'0xae05cd22df81871bc7cc2a04becfb516bfe332c8'}[args.market]
    AMM.FORK.URLS[args.chain]=args.fork_rpc_url or {1:'https://eth.drpc.org',8453:'https://base.drpc.org',42161:'https://arb-mainnet.g.alchemy.com/public'}[args.chain]
    AMM.CHAIN=args.chain;AMM.TOKEN=AMM.FORK.TOKENS[args.chain];AMM.BASE={'wallet':'dev-wallet','network':'evm','chain_id':args.chain}
    work=SOURCE/'work';work.mkdir(mode=0o700,exist_ok=True);work.chmod(0o700)
    evidence=Path(tempfile.mkdtemp(prefix='aave-cli-plan-fork.',dir=work));print(json.dumps({'evidence':str(evidence),'production_writes':0}),flush=True)
    rehearsal=AaveRehearsal(evidence,str(args.chain))
    if args.pin: rehearsal.saved_pin=json.loads(args.pin.read_text())
    try:rehearsal.start();rehearsal.run_lido() if args.market=='lido' else rehearsal.run_etherfi() if args.market=='etherfi' else rehearsal.run_horizon() if args.market=='horizon' else rehearsal.run(positive_only=args.positive_only);print('Aave public CLI named-plan fork passed',args.chain,flush=True)
    finally:rehearsal.close()


if __name__=='__main__':main()
