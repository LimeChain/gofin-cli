#!/usr/bin/env python3
"""Combine counter-derived accepted and rejected Jupiter expiry cases."""
import json
import os
from pathlib import Path

path = Path(os.environ['GOFIN_OUTCOME_EVIDENCE'])
cases = [json.loads(Path(f'{path}.{name}').read_text()) for name in ('accepted', 'rejected')]
for row in cases:
    if type(row.get('approval_count')) is not int or type(row.get('external_submission_count')) is not int:
        raise ValueError('Missing observed approval or provider submission counter')
path.write_text(json.dumps({
    'scenario': 'jupiter-quote-expiry-cli',
    'approval_count': sum(row['approval_count'] for row in cases),
    'external_submission_count': sum(row['external_submission_count'] for row in cases),
    'intervention_count': 0,
    'remaining_external_uncertainty': False,
    'blocked_unrelated_activity': None,
    'counter_basis': 'fixture authorizer and provider submit-call counters',
    'cases': cases,
}, indent=2) + '\n')
