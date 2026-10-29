#!/usr/bin/env python3
"""Coverage and executable-routing regressions for the security gate."""
from copy import deepcopy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('security_runner', Path(__file__).with_name('run-security-tests.py'))
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def row(group='a', package='alpha', target=None, tests=None):
    return {'id': group, 'package': package, 'target': target or ['--lib'],
            'tests': tests or ['checks::bound'], 'reason': 'Reject changed authority.'}


def result(names, passed=None, ignored=0):
    return '\n'.join(f'test {name} ... ok' for name in names) + (
        f'\n\ntest result: ok. {len(names) if passed is None else passed} passed; '
        f'0 failed; {ignored} ignored; 0 measured; 0 filtered out; finished in 0.01s\n')


class CoverageTests(unittest.TestCase):
    def test_required_coverage_cannot_be_reduced(self):
        rows = runner.selections()
        for mutation in ('group', 'test', 'duplicate'):
            changed = deepcopy(rows)
            if mutation == 'group': changed.pop()
            elif mutation == 'test': changed[0]['tests'].pop()
            else: changed.append(changed[0])
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                runner.validate_coverage(changed)

    def test_target_feature_flags_are_preserved_and_unknown_flags_rejected(self):
        self.assertEqual(runner.parse_target(row()), ('--lib', None, ()))
        self.assertEqual(runner.parse_target(row(target=['--lib', '--features', 'binance,kraken,coinbase'])),
                         ('--lib', None, ('binance', 'coinbase', 'kraken')))
        for target in (['--all-features'], ['--lib', '--all-features'], ['--test'],
                       ['--lib', '--release'], ['--bin', 'gofin', '--no-default-features']):
            with self.subTest(target=target), self.assertRaises(ValueError):
                runner.parse_target(row(target=target))

    def test_graph_comparison_keeps_host_and_target_feature_variants(self):
        original = runner.parse_feature_graph('dep v1.0|serde\ndep v1.0|std\n')
        changed = runner.parse_feature_graph('dep v1.0|serde,std\n')
        self.assertFalse(runner.graphs_compatible(original, changed))
        same = runner.parse_feature_graph('dep v1.0|std\ndep v1.0|serde\nextra v1.0|\n')
        self.assertTrue(runner.graphs_compatible(original, same))
        self.assertEqual(runner.merge_feature_graphs([original, same]), same)
        self.assertFalse(runner.graphs_compatible(
            runner.parse_feature_graph('gofin-exchange v0.4.3|binance,kraken\n'),
            runner.parse_feature_graph('gofin-exchange v0.4.3|binance,kraken,test-support\n')))

    def test_selectors_cannot_hide_missing_ambiguous_or_duplicate_tests(self):
        listing = 'checks::bound: test\nother::bound: test\nchecks::proof: test\n\n3 tests, 0 benchmarks\n'
        self.assertEqual(runner.resolve_tests(['checks::bound', 'proof'], listing),
                         ['checks::bound', 'checks::proof'])
        for names in (['bound'], ['absent'], ['proof', 'checks::proof']):
            with self.subTest(names=names), self.assertRaises((ValueError, RuntimeError)):
                runner.resolve_tests(names, listing)

    def test_passing_count_requires_the_exact_executed_identities(self):
        names = ['checks::bound', 'checks::proof']
        self.assertEqual(runner.validate_results(result(names), names), 2)
        for log in (result(['other::bound', 'checks::proof']), result(names, passed=1),
                    result(names, ignored=1), result(['checks::bound', 'checks::bound']),
                    result(names) + result(names), 'test result: ok. 2 passed; 0 failed; 0 ignored;'):
            with self.subTest(log=log), self.assertRaises((ValueError, RuntimeError)):
                runner.validate_results(log, names)


class ArtifactTests(unittest.TestCase):
    def test_executable_is_bound_to_exact_package_target_and_test_profile(self):
        packages = [{'name': 'alpha', 'id': 'path+alpha#0.1.0', 'manifest_path': '/fixture/alpha/Cargo.toml',
                     'targets': [{'name': 'alpha', 'kind': ['lib']}]}]
        artifact = {'reason': 'compiler-artifact', 'package_id': 'path+alpha#0.1.0',
                    'target': {'name': 'alpha', 'kind': ['lib']}, 'profile': {'test': True},
                    'executable': '/fixture/target/debug/deps/alpha-123', 'features': []}
        finished = {'reason': 'build-finished', 'success': True}
        found = runner.match_artifacts([artifact, finished], [row()], packages)
        self.assertEqual(found['a']['cwd'], '/fixture/alpha')
        self.assertEqual(found['a']['executable'], artifact['executable'])
        for field, value in (('package_id', 'path+beta#0.1.0'), ('profile', {'test': False}),
                             ('target', {'name': 'alpha', 'kind': ['bin']}),
                             ('executable', None), ('executable', 'relative-path'), ('features', 'marker')):
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                runner.match_artifacts([{**artifact, field: value}, finished], [row()], packages)
        for messages in ([artifact], [artifact, {'reason': 'build-finished', 'success': False}],
                         [artifact, artifact, finished], [artifact, finished, finished], [None]):
            with self.subTest(messages=messages), self.assertRaises(ValueError):
                runner.match_artifacts(messages, [row()], packages)

    def test_build_command_limits_targets_and_namespaces_features(self):
        command = runner.build_command([row(), row('integration', target=['--test', 'boundary']),
                                       row('transport', 'exchange', ['--lib', '--features', 'binance'])])
        self.assertIn('--no-run', command)
        self.assertIn('--message-format=json', command)
        self.assertIn('exchange/binance', command)
        self.assertNotIn('--all-features', command)
        self.assertNotIn('--workspace', command)
        self.assertNotIn('--all-targets', command)
        self.assertEqual(command.count('--lib'), 1)
        self.assertEqual(command[command.index('--test')+1], 'boundary')


class CargoIntegrationTests(unittest.TestCase):
    def test_real_binaries_preserve_cwd_features_and_shared_listing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'Cargo.toml').write_text('[workspace]\nmembers=["alpha","beta"]\nresolver="3"\n')
            for package in ('alpha', 'beta'):
                (root/package/'src').mkdir(parents=True)
                (root/package/'Cargo.toml').write_text(
                    f'[package]\nname="{package}"\nversion="0.1.0"\nedition="2021"\n'
                    '[features]\nmarker=[]\n')
                (root/package/'sentinel').write_text(package)
                (root/package/'src/lib.rs').write_text('''
#[cfg(test)] mod checks {
    #[test] fn bound() {
        assert_eq!(std::fs::read_to_string("sentinel").unwrap(), env!("CARGO_PKG_NAME"));
    }
    #[test] fn proof() { assert!(!cfg!(feature = "marker")); }
    #[cfg(feature = "marker")] #[test] fn feature_required() { assert!(cfg!(feature = "marker")); }
    #[test] #[ignore] fn ignored_authority() { panic!("ignored tests cannot count as coverage"); }
}
''')
            (root/'alpha/tests').mkdir()
            (root/'alpha/tests/protocol.rs').write_text('#[test] fn boundary() { assert_eq!(std::fs::read_to_string("sentinel").unwrap(), "alpha"); }\n')
            subprocess.run(['cargo', 'generate-lockfile', '--offline'], cwd=root, check=True, capture_output=True)
            subprocess.run(['git', 'init', '-q'], cwd=root, check=True)
            subprocess.run(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                            'commit', '--allow-empty', '-qm', 'fixture'], cwd=root, check=True)
            selected = [row('alpha-bound'), row('alpha-proof', tests=['checks::proof']),
                        row('alpha-integration', target=['--test', 'protocol'], tests=['boundary']),
                        row('beta-bound', 'beta'),
                        row('alpha-marker', target=['--lib', '--features', 'marker'], tests=['checks::feature_required'])]
            env = {**os.environ, 'CARGO_TARGET_DIR': str(root/'target'), 'CARGO_NET_OFFLINE': 'true'}
            env.pop('GITHUB_STEP_SUMMARY', None)
            with patch.object(runner, 'ROOT', root), patch.object(runner, 'selections', return_value=selected), \
                 patch.object(sys, 'argv', ['run-security-tests.py']), patch.dict(os.environ, env, clear=True):
                self.assertEqual(runner.main(), 0)
                report_path = next((root/'.context/test-runs').glob('security-*/report.json'))
                report = json.loads(report_path.read_text())
                self.assertEqual(report['status'], 'passed')
                self.assertEqual(sum(g['passed_tests'] for g in report['groups']), 5)
                # Shared alpha library must be listed once, despite two groups.
                listings = [command for command in report['preparation'] if command['phase'] == 'listing']
                self.assertEqual(len(listings), 4)
                self.assertEqual(sum(command['phase'] == 'build' for command in report['preparation']), 2)
                self.assertEqual(set(report['timings']), {'feature_resolution', 'build', 'listing', 'execution'})
                selected[:] = [row('ignored', tests=['checks::ignored_authority'])]
                self.assertEqual(runner.main(), 1)
                reports = sorted((root/'.context/test-runs').glob('security-*/report.json'))
                self.assertEqual(json.loads(reports[-1].read_text())['status'], 'failed')
                selected[:] = [row('duplicate-one'), row('duplicate-two')]
                self.assertEqual(runner.main(), 1)
                reports = sorted((root/'.context/test-runs').glob('security-*/report.json'))
                duplicate = json.loads(reports[-1].read_text())
                self.assertIn('multiple groups', duplicate['preparation_error'])
                self.assertTrue(all(g['status'] == 'not_run' for g in duplicate['groups']))
                selected[:] = [row('alpha-bound'), row('beta-bound', 'beta')]
                parse_graph = runner.parse_feature_graph
                calls = 0
                def changed_combined_graph(text):
                    nonlocal calls
                    calls += 1
                    graph = parse_graph(text)
                    if calls == 3:
                        graph['unexpected-dependency v1.0'] = {frozenset({'broadened'})}
                    return graph
                with patch.object(runner, 'parse_feature_graph', side_effect=changed_combined_graph):
                    self.assertEqual(runner.main(), 1)
                reports = sorted((root/'.context/test-runs').glob('security-*/report.json'))
                incompatible = json.loads(reports[-1].read_text())
                self.assertIn('changes resolved dependency features', incompatible['preparation_error'])
                self.assertFalse(any(c['phase'] == 'build' for c in incompatible['preparation']))
                self.assertTrue(all(g['status'] == 'not_run' for g in incompatible['groups']))


if __name__ == '__main__':
    unittest.main(verbosity=2)
