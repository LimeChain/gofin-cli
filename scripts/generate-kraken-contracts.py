#!/usr/bin/env python3
"""Generate reviewed, closed Kraken endpoint identities from pinned source inventories.
No network access. Input OpenAPI snapshots are separately versioned.
"""
from pathlib import Path
import json,re
ROOT=Path(__file__).resolve().parents[1]
DOC=ROOT/'docs/kraken'

def snake(s):
 s=re.sub(r'([A-Z]+)([A-Z][a-z])',r'\1_\2',s)
 s=re.sub(r'([a-z0-9])([A-Z])',r'\1_\2',s)
 return re.sub(r'[^a-zA-Z0-9]+','_',s).strip('_').lower()

def pascal(s): return ''.join(x.title() for x in s.split('_'))

def resolve(d,s):
 if '$ref' in s:
  v=d
  for k in s['$ref'].split('/')[1:]: v=v[k.replace('~1','/').replace('~0','~')]
  return resolve(d,v)
 return s

def expand(d,s,seen=()):
 if '$ref' in s:
  if s['$ref'] in seen: raise ValueError('recursive input schema')
  return expand(d,resolve(d,s),seen+(s['$ref'],))
 out={k:v for k,v in s.items() if k not in ('example','examples','xml','discriminator')}
 for k in ('properties',):
  if k in out: out[k]={n:expand(d,v,seen) for n,v in out[k].items()}
 for k in ('allOf','oneOf','anyOf'):
  if k in out: out[k]=[expand(d,v,seen) for v in out[k]]
 # A native string/number union becomes overlapping string branches when
 # financial JSON numbers are adapted to exact decimal text. Preserve its
 # accepted values with anyOf, without weakening tagged object unions.
 if 'oneOf' in out:
  native_types={resolve(d,v).get('type') for v in s.get('oneOf',[]) if isinstance(resolve(d,v).get('type'),str)}
  if 'number' in native_types and 'string' in native_types:
   out['anyOf']=out.pop('oneOf')
 if 'allOf' in out:
  common={'type':'object','properties':{},'required':[],'additionalProperties':False}
  alternatives=None
  for part in out.pop('allOf'):
   if 'oneOf' in part:
    if alternatives is not None: raise ValueError('multiple allOf unions require explicit binding')
    alternatives=part['oneOf']
   else:
    common['properties'].update(part.get('properties',{}));common['required']+=part.get('required',[])
  if alternatives:
   out['oneOf']=[dict(choice,properties={**choice.get('properties',{}),**common['properties']},required=list(dict.fromkeys(choice.get('required',[])+common['required'])),additionalProperties=False) for choice in alternatives]
  else: out.update(common)
 if 'items' in out: out['items']=expand(d,out['items'],seen)
 if isinstance(out.get('additionalProperties'),dict): out['additionalProperties']=expand(d,out['additionalProperties'],seen)
 # OpenAPI 3.0 boolean exclusivity becomes JSON Schema numeric exclusivity.
 for exclusive,bound in (('exclusiveMinimum','minimum'),('exclusiveMaximum','maximum')):
  if isinstance(out.get(exclusive),bool):
   enabled=out.pop(exclusive)
   if enabled and bound in out: out[exclusive]=out.pop(bound)
 # API quantities must remain decimal strings; integer timestamps/options are retained.
 if out.get('type')=='number':
  out['type']='string'; out.pop('format',None)
  out['pattern']=r'^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$'
  for n in ('minimum','maximum','exclusiveMinimum','exclusiveMaximum','multipleOf'): out.pop(n,None)
 if out.get('type')=='integer' and out.get('format') in ('int32','uint32','int64','uint64'):
  bits=32 if out['format'].endswith('32') else 64
  unsigned=out['format'].startswith('uint')
  lower=0 if unsigned else -(2**(bits-1))
  upper=2**bits-1 if unsigned else 2**(bits-1)-1
  out['minimum']=max(out.get('minimum',lower),lower)
  out['maximum']=min(out.get('maximum',upper),upper)
 if out.get('type')=='object' or 'properties' in out:
  out.setdefault('type','object'); out.setdefault('additionalProperties',False)
 if out.pop('nullable',False):
  if isinstance(out.get('type'),str): out['type']=[out['type'],'null']
  if 'enum' in out and None not in out['enum']: out['enum'].append(None)
 return out

specs={n:json.loads((DOC/(n+'.json')).read_text()) for n in ('spot-rest','futures-rest','futures-history-rest','futures-charts-rest','futures-auth-rest')}
lookup={}
for name,d in specs.items():
 if name=='futures-auth-rest': continue
 for path,vs in d['paths'].items():
  for verb,op in vs.items():
   if verb not in ('get','post','put','delete'): continue
   norm=snake(path)
   if name=='spot-rest': prefix='kraken'
   else:
    prefix='krakenfutures'
    scope={'futures-rest':'private','futures-history-rest':'history','futures-charts-rest':'charts'}[name]
    if name=='futures-rest' and path.split('/')[1] in ('feeschedules','instruments','orderbook','tickers','history','historicalfundingrates','historical-funding-rates'): scope='public' if path != '/feeschedules/volumes' else 'private'
    norm=scope+'_'+verb+'_'+norm
   lookup[(prefix,norm if prefix=='krakenfutures' else norm.replace('_'+verb+'_','_'))]=(name,path,verb,op)

# Source method spelling uses symbol/interval while current docs use tradeable/resolution.
alias={'history_get_market_symbol_orders':'history_get_market_tradeable_orders','history_get_market_symbol_executions':'history_get_market_tradeable_executions','history_get_market_symbol_price':'history_get_market_tradeable_price','charts_get_price_type_symbol_interval':'charts_get_tick_type_symbol_resolution','public_get_historicalfundingrates':'public_get_historical_funding_rates','private_get_subaccount_uid_trading_enabled':'private_get_subaccount_subaccount_uid_trading_enabled','private_put_subaccount_uid_trading_enabled':'private_put_subaccount_subaccount_uid_trading_enabled','private_get_orders_status':'private_post_orders_status'}
admin_fragments=('create_subaccount','add_export','remove_export','addresses_post','addresses_id_put','addresses_id_delete','assignmentprogram_delete','pnlpreferences_put','leveragepreferences_put','self_trade_strategy_put','subaccount_uid_trading_enabled_put','rfq_assignment_max_leverage_put','rfq_assignment_max_leverage_delete')
records=[];missing=[]
for client in ('kraken','krakenfutures'):
 inv=json.loads((DOC/(client+'-ccxt.json')).read_text())
 for method in inv['implicitMethods']:
  excluded=inv['excludedMethods'].get(method)
  key=method if client=='krakenfutures' else re.sub(r'_(?:get|post)_', '_', method, count=1)
  found=lookup.get((client,alias.get(key,key)))
  if method.startswith('zendesk_'):
   found=None; excluded='CCXT support-article helper; internal currency metadata, no financial operation'
  if method == 'private_get_recentorders':
   found=('futures-rest','/recentorders','get',{'operationId':'getRecentOrders','deprecated':True})
  if found is None and not excluded: missing.append((client,method))
  if not found:
   records.append({'client':client,'method':method,'excluded':excluded,'missingSchema':not bool(excluded)}); continue
  name,path,verb,op=found; d=specs[name]
  schema={'type':'object','properties':{},'required':[],'additionalProperties':False}
  for raw in specs[name]['paths'].get(path,{}).get('parameters',[]) + op.get('parameters',[]):
   p=resolve(d,raw)
   if p['in']=='header' or p['name'] in ('nonce','otp'): continue
   schema['properties'][p['name']]=expand(d,p.get('schema',{}))
   if p.get('required'): schema['required'].append(p['name'])
  body=resolve(d,op.get('requestBody',{}))
  for media,b in body.get('content',{}).items():
   s=expand(d,b['schema'])
   if s.get('properties'):
    for k,v in s['properties'].items():
     if k not in ('nonce','otp'): schema['properties'][k]=v
    schema['required']+= [k for k in s.get('required',[]) if k not in ('nonce','otp')]
   else: schema['properties']['body']=s; schema['required'].append('body')
   break
  op_name=client+'.'+snake(op.get('operationId',method)).replace('_','-')
  mutating=verb!='get' and not (path.startswith('/private/') and path.split('/')[-1] in ('Level3','Balance','BalanceEx','CreditLines','TradeBalance','OpenOrders','ClosedOrders','QueryOrders','OrderAmends','TradesHistory','QueryTrades','OpenPositions','Ledgers','QueryLedgers','TradeVolume','ExportStatus','RetrieveExport','GetApiKeyInfo','ListWalletAccounts','DepositMethods','DepositStatus','WithdrawMethods','WithdrawAddresses','WithdrawInfo','WithdrawStatus','Strategies','Allocations','AllocateStatus','DeallocateStatus'))
  if client == 'krakenfutures' and path == '/orders/status': mutating=False
  if path.endswith('/DepositAddresses'): schema['properties'].pop('new',None); schema['required']=[k for k in schema['required'] if k!='new']; mutating=False # new=true is a separate write operation
  admin=mutating and any(x in (snake(path)+'_'+verb) or x in method for x in admin_fragments)
  records.append({'client':client,'method':method,'variant':pascal(client+'_'+method),'operation':op_name,'sourceSpec':name,'path':path,'verb':verb.upper(),'schema':schema,'mutating':mutating,'administrative':admin,'excluded':excluded,'description':op.get('summary',op.get('operationId',method)),'deprecated':bool(op.get('deprecated'))})
# A JSON-encoded batch string is not a typed GoFin input. Expose the reviewed
# three order variants structurally, then encode only inside the transport.
by_method={r['method']:r for r in records if r.get('variant')}
batch=by_method['private_post_batchorder']['schema']
choices=[]
for tag,native_schema in [('send','BatchOrderSend'),('edit','BatchOrderEdit'),('cancel','BatchOrderCancel')]:
 item=expand(specs['futures-rest'],{'$ref':'#/components/schemas/'+native_schema})
 # Native batch members have their own field names and options; singleton
 # editorder's orderId is not the batch OrderIdElement's order_id.
 item['properties']['order']={'type':'string','const':tag}
 item['properties']['order_tag']={'type':'string','minLength':1,'maxLength':128}
 item['required']=list(dict.fromkeys(item.get('required',[])+['order','order_tag']))
 if tag in ('edit','cancel'):
  # Both primary descriptions explicitly admit either selector. The edit
  # component's required order_id contradicts that description; preserve the
  # documented client-ID route with a closed, explicit selector alternative.
  item['required']=[key for key in item['required'] if key!='order_id']
  item['anyOf']=[{'required':['order_id']},{'required':['cliOrdId']}]
 choices.append(item)
batch['properties'].pop('json',None);batch['required']=[k for k in batch['required'] if k!='json']+['batchOrder'];batch['properties']['batchOrder']={'type':'array','minItems':1,'maxItems':100,'items':{'oneOf':choices}}
# Official AddOrderBatch examples expose conditional close even though the
# item schema omits it. Use the exact documented singleton close field types,
# a closed nested object, and preserve native JSON batch shape.
spot_order=by_method['private_post_add_order']['schema']
spot_batch_item=by_method['private_post_add_order_batch']['schema']['properties']['orders']['items']
spot_batch_item['properties']['close']={'type':'object','properties':{key[6:-1]:value for key,value in spot_order['properties'].items() if key.startswith('close[')},'required':['ordertype'],'additionalProperties':False}
# New Funding API explicitly authorized as a binding extension.
d=specs['spot-rest']
for path,vs in d['paths'].items():
 if not path.startswith('/funding/'): continue
 for verb,op in vs.items():
  if verb not in ('get','post','put','delete'): continue
  method='funding_'+verb+'_'+snake(path)
  schema={'type':'object','properties':{},'required':[],'additionalProperties':False}
  for raw in op.get('parameters',[]):
   p=resolve(d,raw)
   if p['in']=='header': continue
   schema['properties'][p['name']]=expand(d,p.get('schema',{}))
   if p.get('required'): schema['required'].append(p['name'])
  body=resolve(d,op.get('requestBody',{}))
  for b in body.get('content',{}).values():
   s=expand(d,b['schema']); schema['properties'].update(s.get('properties',{})); schema['required']+=s.get('required',[]); break
  records.append({'client':'kraken','method':method,'variant':pascal('kraken_'+method),'operation':'kraken.'+snake(op['operationId']).replace('_','-'),'sourceSpec':'spot-rest','path':path,'verb':verb.upper(),'schema':schema,'mutating':verb!='get','administrative':'/addresses' in path and verb in ('post','put','delete'),'excluded':None,'description':op.get('summary',op['operationId']),'deprecated':False})
# Legacy address generation is isolated from the read-only discovery contract.
legacy=json.loads(json.dumps(next(r for r in records if r.get('method')=='private_post_deposit_addresses')))
legacy.update(method='private_post_create_deposit_address',variant='KrakenLegacyCreateDepositAddress',operation='kraken.create-deposit-address-legacy',mutating=True,description='Generate a legacy Kraken deposit address',deprecated=True)
legacy['schema']['properties']['new']={'const':True,'type':'boolean'}
legacy['schema']['required'].append('new')
records.append(legacy)
records.append({'client':'krakenfutures','method':'auth_get_api_keys_v3_check','variant':'KrakenfuturesAuthGetApiKeysV3Check','operation':'krakenfutures.check-api-key-v3','sourceSpec':'futures-auth-rest','path':'/api-keys/v3/check','verb':'GET','schema':{'type':'object','properties':{},'required':[],'additionalProperties':False},'mutating':False,'administrative':False,'excluded':None,'description':'Check authenticated Derivatives API key permissions','deprecated':False})
(DOC/'contracts.json').write_text(json.dumps({'ccxtVersion':'4.5.85','specSnapshotDate':'2026-10-10','endpoints':records},indent=2)+'\n')
active=[r for r in records if r.get('variant') and not r.get('excluded')]
inventories={n:json.loads((DOC/(n+'-ccxt.json')).read_text()) for n in ('kraken','krakenfutures')}
for row in active:
 key=row['path'].removeprefix('/private/').removeprefix('/public/').lstrip('/')
 row['ccxtCost']=inventories[row['client']]['endpointCosts'].get(key,1.0 if row['client']=='krakenfutures' else 3.0)
(DOC/'contracts.json').write_text(json.dumps({'ccxtVersion':'4.5.85','specSnapshotDate':'2026-10-10','endpoints':records},indent=2)+'\n')
lines=['//! Closed endpoint identities generated by scripts/generate-kraken-contracts.py.','use serde::{Deserialize, Serialize};','#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]','pub enum KrakenEndpoint {']+[r['variant']+',' for r in active]+['}','impl KrakenEndpoint {','pub const ALL: &[Self] = &[']+['Self::'+r['variant']+',' for r in active]+['];','pub fn method(self) -> &\'static str { match self {']+['Self::'+r['variant']+' => '+json.dumps(r['method'])+',' for r in active]+['}}','pub fn futures(self) -> bool { matches!(self,']+['Self::'+r['variant']+' |' for r in active if r['client']=='krakenfutures']+['Self::'+next(r['variant'] for r in active if r['client']=='krakenfutures')+')}','pub fn mutating(self) -> bool { matches!(self,']+['Self::'+r['variant']+' |' for r in active if r['mutating']]+['Self::'+next(r['variant'] for r in active if r['mutating'])+')}','}']
(ROOT/'crates/gofin-exchange/src/kraken/endpoints.rs').write_text('\n'.join(lines)+'\n')
p=ROOT/'crates/gofin-exchange/src/kraken/endpoints.rs'
s=p.read_text()+'\nimpl KrakenEndpoint {\n'
for key in ('operation','path','verb'):
 s+=f"pub fn {key}(self) -> &'static str {{ match self {{\n"
 for r in active: s+='Self::'+r['variant']+' => '+json.dumps(r[key])+',\n'
 s+='}}\n'
s+='pub fn cost(self)->f64 { match self {\n'+''.join('Self::'+r['variant']+' => '+str(r['ccxtCost'])+',\n' for r in active)+'}}\n'
s+="pub fn query_parameters(self) -> &'static [&'static str] { match self {\n"
for r in active:
 d=specs[r['sourceSpec']];path=d['paths'].get(r['path'],{});op=path.get(r['verb'].lower(),{})
 params=[resolve(d,p) for p in path.get('parameters',[])+op.get('parameters',[])]
 keys=list(dict.fromkeys(p['name'] for p in params if p.get('in')=='query'))
 s+='Self::'+r['variant']+' => &['+','.join(json.dumps(k) for k in keys)+'],\n'
s+='}}\n'

s+="pub fn base(self) -> &'static str { match self {\n"
for r in active:
 name=r['sourceSpec']; d=specs[name]; op=d['paths'].get(r['path'],{}).get(r['verb'].lower(),{})
 servers=op.get('servers',d['servers']); base=servers[0]['url']
 if r['path'].startswith('/funding/'): base='https://api.kraken.com'
 s+='Self::'+r['variant']+' => '+json.dumps(base)+',\n'
s+='}}\npub fn private(self) -> bool { match self {\n'
for r in active:
 private=(r['path'].startswith(('/private/','/funding/')) if r['client']=='kraken' else r['method'].startswith(('private_','auth_')) or (r['method'].startswith('history_') and 'market_' not in r['method']))
 s+='Self::'+r['variant']+' => '+str(private).lower()+',\n'
s+='}}\n}\n';s=re.sub(r'matches!\(self,([\s\S]*?)\)',lambda m:'matches!(self,'+' | '.join(dict.fromkeys(x.strip() for x in m[1].split('|')))+')',s);p.write_text(s)
print('contracts',len(records),'active',len(active),'missing',missing)

# Deserialize each operation through its own generated Rust DTO, then validate the
# richer official constraints (oneOf, patterns, bounds) from the schema snapshot.
types=[]; used=set()
def ident(s):
 s=snake(s) or 'value'
 if s[0].isdigit():s='field_'+s
 return 'r#'+s if s in ('type','ref','self','Self','match','in','where','loop','move','mod','as','use','fn','pub','return','super','crate') else s

def rust_type(schema,name):
 kind=schema.get('type')
 if isinstance(kind,list) and 'null' in kind: return 'serde_json::Value'
 if isinstance(kind,list): kind=next((x for x in kind if x!='null'),None)
 if 'allOf' in schema:
  merged={'type':'object','properties':{},'required':[]}
  for part in schema['allOf']:
   merged['properties'].update(part.get('properties',{}));merged['required']+=part.get('required',[])
  if merged['properties']: schema=merged; kind='object'
 alternatives=schema.get('oneOf',schema.get('anyOf'))
 # Selector assertions (anyOf required order_id/cliOrdId) constrain a single
 # closed object, rather than defining alternative untyped DTO values.
 if 'properties' in schema and alternatives and all(set(s)<= {'required'} for s in alternatives): alternatives=None
 if alternatives:
  variants=[rust_type(s,name+'Choice'+str(i)) for i,s in enumerate(alternatives)]
  if len(set(variants))==1:return variants[0]
  if name not in used:
   compact=lambda t:t if t in ('String','bool','i64','u64','()','serde_json::Value') or t.startswith('Vec<') else 'Box<'+t+'>'
   used.add(name);types.append('#[derive(serde::Deserialize, serde::Serialize)]\n#[serde(untagged)]\npub enum '+name+' {\n'+''.join('Choice'+str(i)+'('+compact(t)+'),\n' for i,t in enumerate(variants))+'}\n')
  return name
 if kind=='object' or 'properties' in schema:
  if name not in used:
   used.add(name);required=set(schema.get('required',[])); fields=[]
   for key,s in schema.get('properties',{}).items():
    t=rust_type(s,name+pascal(snake(key)))
    if key not in required:t='Option<'+t+'>'
    nullable=isinstance(s.get('type'),list) and 'null' in s['type']
    attr='#[serde(rename = '+json.dumps(key)+(', default, skip_serializing_if = "Option::is_none"'+(', deserialize_with = "deserialize_present"' if nullable else '') if key not in required else '')+')]'
    fields.append(attr+'\npub '+ident(key)+': '+t+',')
   types.append('#[derive(serde::Deserialize, serde::Serialize)]\n#[serde(deny_unknown_fields)]\npub struct '+name+' {\n'+'\n'.join(fields)+'\n}\n')
  return name
 if kind=='array':return 'Vec<'+rust_type(schema.get('items',{}),name+'Item')+'>'
 if kind=='boolean':return 'bool'
 if kind=='integer':return 'u64' if str(schema.get('format','')).startswith('uint') or ('minimum' in schema and schema['minimum']>=0) else 'i64'
 if kind=='string':return 'String'
 if kind=='null':return '()'
 # Schema validation still constrains official union/opaque leaves; these never
 # select endpoints or credentials. Their financial interpretation is adapter-owned.
 return 'serde_json::Value'
lines=['//! Operation DTOs generated from pinned official OpenAPI input schemas.','use gofin_exchange::kraken::KrakenEndpoint;','use gofin_domain::{GoFinError, Result};']
branches=[]
for r in active:
 typename=pascal(snake(r['operation']))+'Input'
 t=rust_type(r['schema'],typename)
 branches.append('KrakenEndpoint::'+r['variant']+' => decode::<'+t+'>(value),')
lines+=['fn deserialize_present<\'de,D:serde::Deserializer<\'de>>(deserializer:D)->std::result::Result<Option<serde_json::Value>,D::Error>{ Ok(Some(<serde_json::Value as serde::Deserialize>::deserialize(deserializer)?)) }']
lines+=types
lines+=['fn decode<T: serde::de::DeserializeOwned + serde::Serialize>(value: serde_json::Value) -> Result<serde_json::Value> { let input: T = serde_json::from_value(value).map_err(|_| GoFinError::validation("Kraken operation fields are invalid."))?; serde_json::to_value(input).map_err(|_| GoFinError::validation("Kraken input serialization failed.")) }','pub fn decode_wire(endpoint:KrakenEndpoint,value:serde_json::Value)->Result<serde_json::Value>{match endpoint {']+branches+['}}']
(ROOT/'crates/gofin-kraken/src/inputs.rs').write_text('\n'.join(lines)+'\n')
print('Rust input types',len(types))

# Keep generation reproducible against the maintained Rust formatting contract.
import subprocess
for output in (ROOT/"crates/gofin-kraken/src/inputs.rs", ROOT/"crates/gofin-exchange/src/kraken/endpoints.rs"):
 subprocess.run(["rustfmt", "--edition", "2024", str(output)], check=True)
