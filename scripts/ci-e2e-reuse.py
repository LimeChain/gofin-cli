#!/usr/bin/env python3
"""Reuse verified GitHub E2E evidence for an identical main tree, or run fresh."""
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = '.github/workflows/test.yml'
MAX_AGE = timedelta(hours=24)


def api(path):
    return json.loads(subprocess.check_output(
        ['gh', 'api', path], text=True, timeout=30, stderr=subprocess.PIPE))


def eligible(run, repository, current_run, now):
    updated = datetime.fromisoformat(run['updated_at'].replace('Z', '+00:00'))
    return (run['id'] != current_run and run['path'] == WORKFLOW
            and run['event'] == 'push' and run['status'] == 'completed'
            and run['conclusion'] == 'success'
            and run['head_repository']['full_name'] == repository
            and run['repository']['full_name'] == repository
            and run['head_branch'] and run['head_branch'] != 'main'
            and re.fullmatch(r'[0-9a-f]{40}', run['head_sha'])
            and isinstance(run['run_attempt'], int) and run['run_attempt'] > 0
            and timedelta(0) <= now - updated <= MAX_AGE)


def jobs_passed(jobs, groups, head_sha, now):
    for group in groups:
        matches = [job for job in jobs
                   if job['name'] == f'e2e / Positive CLI scenarios ({group})']
        if len(matches) != 1:
            return False
        job = matches[0]
        if (job['status'] != 'completed' or job['conclusion'] != 'success'
                or job['head_sha'] != head_sha):
            return False
        completed = datetime.fromisoformat(job['completed_at'].replace('Z', '+00:00'))
        if not timedelta(0) <= now - completed <= MAX_AGE:
            return False
        required = ['Run required positive scenarios']
        if group == 'core':
            required.append('Verify CLI regressions are detected and restored')
        for name in required:
            steps = [step for step in job['steps'] if step['name'] == name]
            if len(steps) != 1 or steps[0]['conclusion'] != 'success':
                return False
    return True


def find_reusable(repository, current_run, tree, groups, request=api, now=None):
    now = now or datetime.now(timezone.utc)
    prefix = f'repos/{repository}'
    runs = request(f'{prefix}/actions/workflows/test.yml/runs?event=push&status=success&per_page=100')['workflow_runs']
    # Bounded search: an older or unavailable result is a miss, never a skip.
    for run in runs:
        if not eligible(run, repository, current_run, now):
            continue
        commit = request(f"{prefix}/git/commits/{run['head_sha']}")
        if commit['tree']['sha'] != tree:
            continue
        attempt = run['run_attempt']
        jobs = []
        page = 1
        while True:
            response = request(f"{prefix}/actions/runs/{run['id']}/attempts/{attempt}/jobs?per_page=100&page={page}")
            jobs.extend(response['jobs'])
            if len(jobs) >= response['total_count']:
                break
            if not response['jobs'] or page >= 10:
                return None
            page += 1
        if not jobs_passed(jobs, groups, run['head_sha'], now):
            continue
        # A rerun may have started while checking jobs. Never reuse stale status.
        latest = request(f"{prefix}/actions/runs/{run['id']}")
        if (eligible(latest, repository, current_run, now)
                and latest['run_attempt'] == attempt
                and latest['head_sha'] == run['head_sha']):
            return {'run_id': str(run['id']), 'source_sha': run['head_sha'],
                    'tree': tree, 'url': f"https://github.com/{repository}/actions/runs/{run['id']}/attempts/{attempt}"}
    return None


def lookup(env, request=api):
    if (env.get('GITHUB_EVENT_NAME') != 'push'
            or env.get('GITHUB_REF') != 'refs/heads/main'
            or env.get('GITHUB_RUN_ATTEMPT') != '1'):
        return None
    repository = env['GITHUB_REPOSITORY']
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('Invalid repository identity')
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    if head != env['GITHUB_SHA']:
        raise ValueError('Checkout differs from workflow source')
    tree = subprocess.check_output(['git', 'rev-parse', 'HEAD^{tree}'], cwd=ROOT, text=True).strip()
    groups = json.loads((ROOT / 'scripts/e2e/required.json').read_text()).keys()
    return find_reusable(repository, int(env['GITHUB_RUN_ID']), tree, groups, request)


def gate_passed(needs):
    required = {'lint', 'tests', 'lifecycle', 'native', 'e2e_lookup', 'e2e'}
    if set(needs) != required:
        return False
    if any(needs[name]['result'] != 'success' for name in required - {'e2e', 'e2e_lookup'}):
        return False
    if needs['e2e']['result'] == 'success':
        return True
    run_id = needs['e2e_lookup'].get('outputs', {}).get('run_id', '')
    return (needs['e2e']['result'] == 'skipped'
            and needs['e2e_lookup']['result'] == 'success'
            and isinstance(run_id, str) and re.fullmatch(r'[1-9][0-9]*', run_id) is not None)


def main():
    if sys.argv[1:] == ['--check-gate']:
        if not gate_passed(json.loads(os.environ['NEEDS'])):
            raise SystemExit('Required checks failed, or E2E was skipped without verified reusable evidence.')
        return
    try:
        evidence = lookup(os.environ)
    except Exception as error:
        print(f'E2E evidence lookup unavailable ({type(error).__name__}); running fresh tests.')
        evidence = None
    with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
        output.write(f"run_id={evidence['run_id'] if evidence else ''}\n")
    if evidence:
        message = (f"Reusing [successful E2E run {evidence['run_id']}]({evidence['url']}) "
                   f"for identical Git tree `{evidence['tree']}`. Source commit: `{evidence['source_sha']}`. "
                   'All required groups and execution steps passed within 24 hours.\n')
    else:
        message = 'No eligible identical-tree E2E result; running all positive scenarios.\n'
    print(message)
    with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
        summary.write(message)


if __name__ == '__main__':
    main()
