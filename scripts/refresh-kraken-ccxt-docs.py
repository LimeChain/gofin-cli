#!/usr/bin/env python3
"""Refresh parameter comments only from the exact installed CCXT source snapshot.
No network access; preserve reviewed transitive endpoint/alias mappings.
"""
import argparse,hashlib,json,re
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--source-dir',type=Path,required=True);args=p.parse_args()
root=Path(__file__).resolve().parents[1]
for client in ('kraken','krakenfutures'):
 path=root/'docs/kraken'/f'{client}-ccxt.json';inv=json.loads(path.read_text());source=(args.source_dir/f'{client}.rs').read_text()
 assert hashlib.sha256(source.encode()).hexdigest()==inv['sourceSha256'], 'Pinned CCXT source hash mismatch'
 for method,binding in inv['unifiedBindings'].items():
  match=re.search(r'\bpub async fn '+re.escape(method)+r'\(',source)
  assert match,method
  prefix=source[:match.start()].rstrip();end=prefix.rfind('*/');start=prefix.rfind('/*',0,end)
  comment=prefix[start:end] if end>=0 and not prefix[end+2:].strip() else ''
  binding['ccxtParameterDeclarations']=[line.strip().removeprefix('*').strip() for line in comment.splitlines() if '@param' in line]
 path.write_text(json.dumps(inv,indent=2)+'\n')
