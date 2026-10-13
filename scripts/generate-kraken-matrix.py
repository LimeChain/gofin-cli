#!/usr/bin/env python3
"""Generate versioned coverage/option inventory from pinned schemas and reviewed mappings."""
import json,re
from pathlib import Path
root=Path(__file__).resolve().parents[1];doc=root/'docs/kraken'
contracts=json.loads((doc/'contracts.json').read_text());endpoints=contracts['endpoints']
catalog={d['name']:d for d in json.loads((root/'crates/fin-cli/data/financial-catalog.json').read_text())}
clients={name:json.loads((doc/(name+'-ccxt.json')).read_text()) for name in ('kraken','krakenfutures')}
specs={name:json.loads((doc/(name+'.json')).read_text()) for name in ('spot-rest','futures-rest','futures-history-rest','futures-charts-rest','futures-auth-rest')}
read_evidence=json.loads((doc/'read-verification.json').read_text())
mutation_evidence=json.loads((doc/'mutation-verification.json').read_text())
native_evidence={(row['client'],row['method']):row for report in (read_evidence,mutation_evidence) for row in report['endpoints']}
option_audit=json.loads((doc/'unified-options.json').read_text())
unified_audits={(entry['client'],entry['method']):entry for entry in option_audit['overrides']}
def permission_evidence(endpoint):
 operation=specs.get(endpoint.get('sourceSpec'),{}).get('paths',{}).get(endpoint.get('path'),{}).get(endpoint.get('verb','').lower(),{})
 description=operation.get('description','')
 labels=re.findall(r'API Key Permissions Required:\*\*\s*`([^`]+)`',description)
 labels+=re.findall(r'Requires the `([^`]+)` API key permission',description)
 if labels:return {'sourceSpec':endpoint.get('sourceSpec'),'sourcePath':endpoint.get('path'),'documented':labels}
 return {'sourceSpec':endpoint.get('sourceSpec'),'sourcePath':endpoint.get('path'),'documented':[],'status':'No explicit permission declaration in this pinned operation description'}
def paths(schema,prefix=''):
 out=[]
 for name,child in schema.get('properties',{}).items():
  path=prefix+'/'+name.replace('~','~0').replace('/','~1');out.append({'inputPointer':path,'type':child.get('type','union'),'required':name in schema.get('required',[]),'enum':child.get('enum'),'default':child.get('default'),'nativeBinding':path})
  out+=paths(child,path)
  if 'items' in child:out+=paths(child['items'],path+'/*')
 for union in ('oneOf','anyOf'):
  for i,child in enumerate(schema.get(union,[])):out+=paths(child,prefix)
 return out
rows=[]
for endpoint in endpoints:
 r={k:v for k,v in endpoint.items() if k!='schema'}
 r['optionMappings']=paths(endpoint.get('schema',{}))
 definition=catalog.get(r.get('operation'),{})
 native=endpoint.get('schema',{}).get('properties',{})
 r['finControlMappings']=paths({'properties':{k:v for k,v in definition.get('input_schema',{}).get('properties',{}).items() if k not in native},'required':definition.get('input_schema',{}).get('required',[])})
 for control in r['finControlMappings']:control['nativeBinding']=None;control['binding']='Protected Fin account/approval/policy/verification control; never passthrough to venue'
 r['outputSchema']=definition.get('output_schema')
 r['examples']=definition.get('examples',[])
 r['product']='Spot/Margin' if r['client']=='kraken' else 'Derivatives'
 if endpoint.get('excluded'):
  r.update(authorization='excluded',implementation='excluded',settlementEvidence='none',verification={'inventory':'checked','synthetic':'excluded','fork':'not_run','readOnlyLive':'not_run','liveWrite':'not_run','nativeTouchId':'not_run'})
 else:
  admin=endpoint['administrative'];write=endpoint['mutating'];artifact='retrieve_export' in endpoint['method'] or 'accountlogcsv' in endpoint['method']
  revision=endpoint['method'] in ('private_post_amend_order','private_post_edit_order','private_post_editorder')
  r['authorization']='fresh protected human configuration approval; no financial plans' if admin else 'fresh exact financial approval; linked authority revisions cannot use plans or boundary grants' if revision else 'protected exact financial approval or approved supported finite plan scope; linked compound mutations require fresh exact approval' if endpoint['method']=='private_post_batchorder' else 'protected exact financial approval or approved supported finite plan scope' if write else 'no financial-action approval'
  r['permissionEvidence']=permission_evidence(endpoint)
  r['permissions']='; '.join(r['permissionEvidence']['documented']) or ('Derivatives general FULL_ACCESS for writes/configuration; account/product access checked separately' if r['client']=='krakenfutures' and (write or admin) else 'Product-specific authenticated read access' if endpoint['method'].startswith(('private_','history_','auth_','funding_')) else 'Public route; no key')
  if write:r['settlementPermissions']='Additional authenticated balance, order, trade, ledger, allocation or history reads are checked by the financial family; venue mutation permission alone is insufficient.'
  r['implementation']='configuration implemented' if admin else 'protected financial lifecycle present; listed variants and option verification incomplete' if write else 'bounded private artifact output implemented' if artifact else 'typed bounded read implemented'
  r['settlementEvidence']='configuration acknowledgement and available readback; unknown after ambiguous response loss' if admin else 'exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved' if write else 'bounded original bytes, private create-new output and SHA256' if artifact else 'exact bounded venue response; no accounting settlement claim'
  r['verification']={'inventory':'checked','schema':'checked','synthetic':'family fixtures; consult verification report for individual limitations','fork':'not_run','readOnlyLive':'not_run','liveWrite':'not_run','nativeTouchId':'not_run'}
  evidence=native_evidence.get((r['client'],r['method']))
  if evidence:
   r['verification']['independentNativeRequestMapping']={
    'status':'passed-synthetic',
    'requestCases':evidence.get('requestCases',len(evidence.get('cases',[]))),
    'source':'read-verification.json' if not write and not admin else 'mutation-verification.json',
    'evidence':evidence,
    'boundary':'Closed input/DTO/path/query mapping; financial settlement, live permissions and native Touch ID are separate evidence.'}
 rows.append(r)
unified=[]
for client,inventory in clients.items():
 for name,binding in inventory['unifiedBindings'].items():
  unified.append({'client':client,'method':name,**binding,'optionTranslationAudit':unified_audits[(client,name)],'inputContract':'Each mapped operation exposes exact native fields; declarations are translation guidance, not an arbitrary params bag.'})
extra=[]
for name in ('spot-rest','futures-rest','futures-history-rest','futures-charts-rest','futures-auth-rest'):
 spec=specs[name];mapped={(r.get('path'),r.get('verb','').lower()) for r in endpoints if r.get('sourceSpec')==name}
 for path,verbs in spec['paths'].items():
  for verb,operation in verbs.items():
   if verb in ('get','post','put','delete') and (path,verb) not in mapped:extra.append({'sourceSpec':name,'path':path,'verb':verb.upper(),'operationId':operation.get('operationId'),'coverage':'Published schema endpoint outside pinned CCXT method inventory; not exposed or claimed implemented.'})
result={'schemaVersion':1,'ccxtVersion':'4.5.85','snapshotDate':'2026-10-10','endpoints':rows,'unifiedMethods':unified,'inheritedCapabilities':{n:i['capabilityBindings'] for n,i in clients.items()},'inheritedAndEmulatedOptionAudits':option_audit['inheritedAndEmulated'],'publishedOutsidePinnedCcxt':extra,'globalLimitations':['Independent native request mapping covers 92 reads (757 variants) and 39 mutations/configuration routes (405 variants). Financial settlement/option-family fixtures remain distinct and are not exhaustive.','Mixed cash/margin batches and single-member revisions, same-owner derivative edit/cancel groups, composed mutation/new-order batches, finite parent-funded cash sequences and whole-command margin fee admission have native synthetic lifecycle evidence. Issued conditional cash/margin child revisions have bounded native synthetic evidence for atomic audits, legacy lineage, receipt goals, races and restart. Child principal remains within the original generated-credit/full-loan ceiling; expansion beyond it is incomplete.','Tokenized base/rebased financial pricing remains incomplete: the official ticker contract does not establish its exact unit denomination. Default financial workflows reject KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN; no parity, conversion factor or second custody holding is inferred.','Attached conditional parents are unsupported by both native Atomic Amends and legacy EditOrder; generated children have their own capability status. Quote-volume market orders cannot be atomically revised with the native base-quantity field while preserving their finite quote cap.' ,'Options are not supported by the pinned CCXT product client; financial option-market coverage is incomplete. Same-unit signed maker rebates and negative USD liability observation are implemented. Non-USD converted fee legs, deficit repayment/conversion, preferred FEE-credit spending and other negative native currency liabilities remain incomplete; see derivative-fee-units.json.','Legacy submitted funding/deposit scopes lacking an approved fee identity or original fee price remain unknown; retrospective excess accounting is incomplete. Unexpected zero-quoted setup charges without correlated original fee authority are unproven.','Exact noncash Earn receipt plans authorize one substantive finite position-source attempt; short receipts retain accounting and require fresh separate authority for another attempt.','CCXT response normalization/filtering and automatic pagination are caller-side equivalents; unsupported native aliases and upstream source gaps are explicit in unified-options.json.'],'explicitExclusions':['WebSockets/token issuance','API-level 2FA','Assignment-program enrollment']}
(doc/'capability-matrix.json').write_text(json.dumps(result,indent=2)+'\n')
def clean(s):return str(s).replace('|','\\|').replace('\n',' ')
lines=['# Kraken capability and evidence matrix','','Pinned CCXT/Rust **4.5.85**, official schema snapshot **2026-10-10**. This is an implementation inventory, not a full-coverage verification claim. Automated implementation checks passed at the reported checkpoint; unresolved implementation cases remain explicitly incomplete.','','The machine-readable [matrix](capability-matrix.json) records every native option path, aliases, inherited/emulated mappings, source/product, authorization, deprecated APIs, evidence and verification status. [Source manifest](source-manifest.json) pins the underlying sources. [Verification report](verification.md) separates synthetic evidence from fork, live and native Touch ID checks.','','Protected financial actions use the engine/store approval and reconciliation path. Configuration uses `fin configure` with separate fresh human approval; financial plans cannot authorize it. Reads require venue permissions but no financial-action approval. Native acknowledgement alone does not establish financial completion.','','| Client/product | Method / native endpoint | Fin operation | Authorization | Implementation / evidence |','| --- | --- | --- | --- | --- |']
for r in rows:
 native=r['method']+((' · '+r['verb']+' '+r['path']) if r.get('path') else '')
 state=(r.get('excluded') or (r['implementation']+'; '+r['settlementEvidence']))+('; deprecated' if r.get('deprecated') else '')
 lines.append('| '+' | '.join(clean(v) for v in [r['client']+'/'+r['product'],native,r.get('operation','—'),r['authorization'],state])+' |')
lines+=['','## Unified aliases and emulated capabilities','','These methods map to the same protected native operations; CCXT aliases do not create another accounting owner. The JSON matrix preserves exact parameter declarations and native option paths. Automatic CCXT pagination/normalization is not Fin execution authority.','','| Client | Unified method | Fin operations |','| --- | --- | --- |']
for r in unified:lines.append('| '+r['client']+' | '+r['method']+' | '+', '.join(r['operations'])+' |')
lines+=['','Inherited true/emulated capability mappings are recorded explicitly in the JSON matrix, including aliases with no product override.','','## Completeness against newer published schemas','','The pinned implicit CCXT inventory and authorized Funding extension are closed and mapped. Official snapshots also publish endpoints outside that CCXT inventory. They are listed here to make the boundary visible; they are not claimed implemented.','','| Spec | Published endpoint |','| --- | --- |']
for r in extra:lines.append('| '+r['sourceSpec']+' | '+r['verb']+' '+r['path']+' |')
lines+=['','## Remaining coverage limits','']+['- '+s for s in result['globalLimitations']]
(doc/'capability-matrix.md').write_text('\n'.join(lines)+'\n')
print(f'{len(rows)} endpoint records; {len(unified)} unified mappings; {sum(len(r["optionMappings"]) for r in rows)} native option paths; {len(extra)} published endpoints outside pinned CCXT inventory')
