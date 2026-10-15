#!/usr/bin/env python3
"""Independent primary-source mutation request fixtures; never dispatch writes.

Endpoint selection is static/reviewed. The read fixture builder supplies only
raw OpenAPI resolution/sample utilities; no GoFin generated contract is read.
"""
import copy
import hashlib
import json
from pathlib import Path
import build_read_fixtures as raw

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent

original_flatten=raw.flatten

def distributed_flatten(spec,schema):
    schema=original_flatten(spec,schema)
    for union in ('oneOf','anyOf'):
        if union in schema and 'properties' in schema:
            common=schema.pop('properties')
            required=schema.pop('required',[])
            branches=[]
            for branch in schema[union]:
                branch=original_flatten(spec,branch)
                branch['properties']={**common,**branch.get('properties',{})}
                branch['required']=list(dict.fromkeys(required+branch.get('required',[])))
                branches.append(branch)
            schema[union]=branches
    return schema

raw.flatten=distributed_flatten

original_sample=raw.sample

def exact_sample(spec,schema,name='',example=None):
    schema=raw.flatten(spec,schema)
    if 'const' in schema:return schema['const']
    for union in ('oneOf','anyOf'):
        if union in schema and 'properties' not in schema and example is None:
            return exact_sample(spec,schema[union][0],name)
    if schema.get('type') in ('number','string') and isinstance(example,(int,float)):
        example=str(example)
    if schema.get('type') in ('number','string') and isinstance(schema.get('example'),(int,float)):
        schema['example']=str(schema['example'])
    if schema.get('type')=='string' and schema.get('pattern','').startswith('^AB'):
        return 'AB00000-00000-000000'
    if schema.get('type')=='number' and example is None and 'example' not in schema:
        return str(schema.get('minimum',0))
    if schema.get('type')=='object' or 'properties' in schema:
        if example is not None or isinstance(schema.get('example'),dict):
            values=example if example is not None else schema['example']
            return {key:exact_sample(spec,schema.get('properties',{}).get(key,{}),key,value) for key,value in values.items()}
        keys=list(schema.get('required',[]))
        # Published tagged object branches often omit these required keys.
        for key in ('order','current_fee','quoted_fee'):
            if key in schema.get('properties',{}) and key not in keys:keys.append(key)
        if not keys:keys=list(schema.get('properties',{}))[:1]
        return {key:exact_sample(spec,schema['properties'][key],key) for key in keys if key not in raw.EXCLUDED}
    return original_sample(spec,schema,name,example)

raw.sample=exact_sample

def nested_variants(spec,schema,name):
    schema=raw.flatten(spec,schema)
    result=raw.variants(spec,schema,name)
    if schema.get('type')=='number':
        result += [('exact-tiny','0.000000000000000001'),('exact-large','12345678901234567890.000000000000000001')]
    if schema.get('type')=='array':
        for label,value in nested_variants(spec,schema.get('items',{}),name):
            result.append(('child:'+label,[value]*max(1,schema.get('minItems',1))))
    for union in ('oneOf','anyOf'):
        for index,branch in enumerate(schema.get(union,[])):
            result += [(f'{union}:{index}:{label}',value) for label,value in nested_variants(spec,branch,name)]
    # allOf properties need baseline siblings for every meaningful branch.
    if schema.get('type')=='object' or 'properties' in schema:
        baseline=raw.sample(spec,schema,name)
        for key,child in schema.get('properties',{}).items():
            result += [(f'field:{key}:{label}',{**baseline,key:value}) for label,value in nested_variants(spec,child,key)]
    seen=set()
    return [(label,value) for label,value in result if not (json.dumps(value,sort_keys=True) in seen or seen.add(json.dumps(value,sort_keys=True)))]

def option_pointers(spec,schema,prefix):
    schema=raw.flatten(spec,schema)
    result={prefix}
    for union in ('oneOf','anyOf'):
        for branch in schema.get(union,[]):result.update(option_pointers(spec,branch,prefix))
    for name,child in schema.get('properties',{}).items():result.update(option_pointers(spec,child,prefix+'/'+name))
    if schema.get('type')=='array':result.update(option_pointers(spec,schema.get('items',{}),prefix+'/*'))
    return result

def main():
    plan=json.loads((HERE/'mutation-endpoint-plan.json').read_text())['endpoints']
    out={'schemaVersion':1,'provenance':'Independent pinned raw official OpenAPI request schemas; no GoFin generated contracts/DTO inputs',
         'scope':'Mapping and strict typed decoding only; no transport dispatch, financial approval, settlement or live evidence.',
         'sourceInterpretations':['OpenAPI number inputs become exact decimal strings without rounding; native integer/boolean/null retain type. Decimal business limits require separate financial validation.',
          'Futures batchOrder is the logical object encoded into official json query text by the reviewed signer; processBefore and broker retain query placement.',
          'GoFin requires explicit order_tag on every Futures batch child for immutable proof ownership, although raw edit/cancel branches do not require the tag.',
          'AllOf sibling properties and required keys are distributed through meaningful object union branches; current_fee versus quoted_fee uses its explicit source discriminator.',
          'Legacy new=true is a dedicated protected endpoint alias; discovery removes new and cannot generate an address.'],
         'excludedInputs':['nonce','otp'],'endpoints':[]}
    for endpoint in plan:
        source=ROOT/(endpoint['sourceSpec']+'.json')
        spec=json.loads(source.read_text())
        raw.EXCLUDED={'nonce','otp'} if endpoint['method']=='private_post_create_deposit_address' else {'nonce','otp','new'}
        logical=copy.deepcopy(spec)
        if endpoint['method']=='private_post_batchorder':
            op=logical['paths'][endpoint['path']][endpoint['verb'].lower()]
            op['parameters']=[raw.resolve(logical,p) for p in op['parameters']]
            batch=next(p for p in op['parameters'] if p.get('name')=='json')
            op['parameters']=[p for p in op['parameters'] if p['name']!='json']
            op['requestBody']={'content':batch['content']}
        operation,properties,query,cases=raw.requests(logical,endpoint)
        baseline=cases[0]['native']
        cases=[cases[0]]+[{'case':name+':'+label,'option':name,'native':{**baseline,name:value}}
            for name,schema in properties.items() for label,value in nested_variants(logical,schema,name)]
        if endpoint['method']=='private_post_batchorder':
            for case in cases:
                for index,child in enumerate(case['native']['batchOrder']):child['order_tag']=str(index+1)
        if endpoint['method']=='private_post_create_deposit_address':
            properties['new']={'type':'boolean','enum':[True]}
            for case in cases:
                case['native']['new']=True
            cases=[c for c in cases if c.get('option')!='new']+[{'case':'new:true','option':'new','native':cases[0]['native'].copy()}]
        out['endpoints'].append({**endpoint,'sourceSha256':hashlib.sha256(source.read_bytes()).hexdigest(),
           'operationId':operation.get('operationId'),'optionPointers':sorted({p for name,schema in properties.items() for p in option_pointers(logical,schema,'/'+name)}),'options':sorted(properties),'query':query+(['json'] if endpoint['method']=='private_post_batchorder' else []),'cases':cases,
           'responseExamples':raw.response_examples(spec,endpoint)})
    out['totals']={'endpoints':len(out['endpoints']),'options':sum(len(e['options']) for e in out['endpoints']),
        'cases':sum(len(e['cases']) for e in out['endpoints']), 'nestedOptionPointers':sum(len(e['optionPointers']) for e in out['endpoints'])}
    (HERE/'mutation-native-options.json').write_text(json.dumps(out,indent=2)+'\n')
    print(json.dumps(out['totals']))

if __name__=='__main__':main()
