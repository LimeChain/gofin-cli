#!/usr/bin/env python3
"""Synthetic CI evidence tests: unsafe or incomplete evidence must run fresh."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('reuse', Path(__file__).with_name('ci-e2e-reuse.py'))
reuse = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(reuse)

NOW = datetime(2026, 10, 28, 12, tzinfo=timezone.utc)
STAMP = NOW.isoformat()
SHA = 'a' * 40
TREE = 'b' * 40
REPO = 'example/gofin'
GROUPS = json.loads((reuse.ROOT / 'scripts/e2e/required.json').read_text()).keys()


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.run = {'id': 123, 'path': reuse.WORKFLOW, 'event': 'push',
                    'status': 'completed', 'conclusion': 'success', 'head_sha': SHA,
                    'head_branch': 'feature', 'head_repository': {'full_name': REPO},
                    'repository': {'full_name': REPO}, 'run_attempt': 1, 'updated_at': STAMP}
        self.jobs = []
        for group in GROUPS:
            steps = [{'name': 'Run required positive scenarios', 'conclusion': 'success'}]
            if group == 'core':
                steps.append({'name': 'Verify CLI regressions are detected and restored', 'conclusion': 'success'})
            self.jobs.append({'name': f'e2e / Positive CLI scenarios ({group})',
                              'status': 'completed', 'conclusion': 'success',
                              'head_sha': SHA, 'completed_at': STAMP, 'steps': steps})
        self.tree = TREE
        self.latest = None
        self.requests = []

    def api(self, path):
        self.requests.append(path)
        if '/workflows/test.yml/runs?' in path:
            return {'workflow_runs': [self.run]}
        if '/git/commits/' in path:
            return {'tree': {'sha': self.tree}}
        if '/attempts/1/jobs?' in path:
            return {'jobs': self.jobs, 'total_count': len(self.jobs)}
        if path.endswith('/actions/runs/123'):
            return self.latest or self.run
        self.fail(f'Unexpected API call: {path}')

    def find(self):
        return reuse.find_reusable(REPO, 999, TREE, GROUPS, self.api, NOW)

    def test_identical_tree_reuses_success_across_commit_ids(self):
        found = self.find()
        self.assertEqual(found['run_id'], '123')
        self.assertEqual(found['tree'], TREE)
        self.assertTrue(found['url'].endswith('/123/attempts/1'))

    def test_every_tree_change_requires_fresh_tests(self):
        self.tree = 'c' * 40
        self.assertIsNone(self.find())
        self.assertFalse(any('/jobs?' in path for path in self.requests))

    def test_rejects_untrusted_incomplete_or_old_runs(self):
        changes = [{'event': 'pull_request_target'}, {'event': 'workflow_dispatch'},
                   {'status': 'in_progress'}, {'conclusion': 'failure'},
                   {'head_branch': 'main'}, {'id': 999}, {'path': '.github/workflows/other.yml'},
                   {'head_repository': {'full_name': 'fork/gofin'}},
                   {'repository': {'full_name': 'fork/gofin'}},
                   {'updated_at': (NOW-timedelta(hours=25)).isoformat()},
                   {'updated_at': (NOW+timedelta(hours=1)).isoformat()},
                   {'head_sha': 'invalid'}, {'run_attempt': 0}]
        original = deepcopy(self.run)
        for change in changes:
            with self.subTest(change=change):
                self.run = {**deepcopy(original), **change}
                self.assertIsNone(self.find())

    def test_missing_duplicate_skipped_failed_or_wrong_source_group_rejected(self):
        original = deepcopy(self.jobs)
        for mutation in ('missing', 'duplicate', 'skipped', 'failed', 'source', 'old', 'step', 'mutation'):
            with self.subTest(mutation=mutation):
                self.jobs = deepcopy(original)
                if mutation == 'missing': self.jobs.pop()
                elif mutation == 'duplicate': self.jobs.append(self.jobs[-1])
                elif mutation == 'skipped': self.jobs[-1]['conclusion'] = 'skipped'
                elif mutation == 'failed': self.jobs[-1]['conclusion'] = 'failure'
                elif mutation == 'source': self.jobs[-1]['head_sha'] = 'd' * 40
                elif mutation == 'old': self.jobs[-1]['completed_at'] = (NOW-timedelta(hours=25)).isoformat()
                elif mutation == 'step': self.jobs[-1]['steps'][0]['conclusion'] = 'skipped'
                elif mutation == 'mutation': self.jobs[0]['steps'][-1]['conclusion'] = 'failure'
                self.assertIsNone(self.find())

    def test_rerun_race_does_not_reuse_old_success(self):
        self.latest = {**self.run, 'run_attempt': 2, 'status': 'in_progress', 'conclusion': None}
        self.assertIsNone(self.find())

    def test_jobs_pagination_preserves_all_required_groups(self):
        def paginated(path):
            if '/jobs?' in path:
                jobs = self.jobs[:2] if path.endswith('page=1') else self.jobs[2:]
                return {'jobs': jobs, 'total_count': len(self.jobs)}
            return self.api(path)
        self.assertIsNotNone(reuse.find_reusable(REPO, 999, TREE, GROUPS, paginated, NOW))

    def test_non_main_manual_release_and_rerun_bypass_lookup(self):
        env = {'GITHUB_EVENT_NAME': 'push', 'GITHUB_REF': 'refs/heads/main', 'GITHUB_RUN_ATTEMPT': '1'}
        for change in ({'GITHUB_REF': 'refs/heads/feature'}, {'GITHUB_EVENT_NAME': 'workflow_dispatch'},
                       {'GITHUB_EVENT_NAME': 'release'}, {'GITHUB_RUN_ATTEMPT': '2'}):
            with self.subTest(change=change):
                self.assertIsNone(reuse.lookup({**env, **change}, lambda _: self.fail('API must not be called')))

    def test_api_failure_emits_cache_miss(self):
        with tempfile.TemporaryDirectory() as temp:
            output, summary = Path(temp)/'output', Path(temp)/'summary'
            with patch.object(reuse, 'lookup', side_effect=RuntimeError('unavailable')), \
                 patch.object(reuse.sys, 'argv', ['ci-e2e-reuse.py']), \
                 patch.dict(reuse.os.environ, GITHUB_OUTPUT=str(output), GITHUB_STEP_SUMMARY=str(summary)):
                reuse.main()
            self.assertEqual(output.read_text(), 'run_id=\n')
            self.assertIn('running all positive scenarios', summary.read_text())

    def test_required_gate_allows_only_proven_e2e_skip(self):
        needs = {name: {'result': 'success'} for name in ('lint','tests','lifecycle','native','e2e_lookup','e2e')}
        self.assertTrue(reuse.gate_passed(needs))
        needs['e2e_lookup']['result'] = 'failure'
        self.assertTrue(reuse.gate_passed(needs))  # Fresh E2Es supersede a failed optional lookup.
        needs['e2e_lookup']['result'] = 'success'
        needs['e2e']['result'] = 'skipped'
        self.assertFalse(reuse.gate_passed(needs))
        needs['e2e_lookup']['outputs'] = {'run_id': '123'}
        self.assertTrue(reuse.gate_passed(needs))
        for name in needs:
            for result in ('failure','cancelled','skipped'):
                if name == 'e2e' and result == 'skipped': continue
                with self.subTest(name=name,result=result):
                    changed = deepcopy(needs); changed[name]['result'] = result
                    self.assertFalse(reuse.gate_passed(changed))
        for run_id in ('', '0', 'true', '123\n456'):
            needs['e2e_lookup']['outputs']['run_id'] = run_id
            self.assertFalse(reuse.gate_passed(needs))


if __name__ == '__main__':
    unittest.main(verbosity=2)
