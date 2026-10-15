#!/usr/bin/env python3
"""Offline beta selection and static publication artifact checks."""
import copy
import ast
import importlib.util
import tempfile
import re
from pathlib import Path
import unittest

REPO = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('installer_site', REPO / 'scripts/build-installer-site.py')
site = importlib.util.module_from_spec(spec)
spec.loader.exec_module(site)


def release(tag='v0.2.0-beta.1', published='2026-01-01T00:00:00Z'):
    names = ['install.sh', 'SHA256SUMS'] + [f'gofin-{tag}-{target}.tar.gz' for target in site.TARGETS]
    return {'tag_name': tag, 'draft': False, 'prerelease': True, 'published_at': published,
            'assets': [{'name': name, 'size': 100, 'state': 'uploaded',
                        'browser_download_url': f'https://github.com/{site.REPOSITORY}/releases/download/{tag}/{name}'} for name in names]}


class InstallerSiteTests(unittest.TestCase):
    def test_newest_completed_beta_wins_regardless_of_api_order(self):
        old = release()
        new = release('v0.2.0-beta2', '2026-01-03T00:00:00Z')
        for releases in [[new, old], [old, new]]:
            self.assertEqual(site.select_beta(releases), new['tag_name'])

    def test_drafts_stable_alpha_and_incomplete_releases_do_not_replace_ready_beta(self):
        ready = release()
        for mutation in [dict(draft=True), dict(prerelease=False), dict(tag_name='v0.3.0'),
                         dict(tag_name='v0.3.0-alpha.1'), dict(tag_name='v0.3.0-betamax'),
                         dict(tag_name='v0.3.0-beta.01'), dict(tag_name="v0.3.0-beta';exit"),
                         dict(assets=[]), dict(published_at=None), dict(published_at='bad')]:
            candidate = release('v0.3.0-beta.1', '2026-01-03T00:00:00Z')
            candidate.update(mutation)
            with self.subTest(mutation=mutation):
                self.assertEqual(site.select_beta([candidate, ready]), ready['tag_name'])

    def test_missing_duplicate_empty_pending_and_foreign_assets_are_not_ready(self):
        for mode in ['missing', 'duplicate', 'empty', 'pending', 'foreign', 'old-fin-names']:
            candidate = release()
            if mode == 'missing': candidate['assets'].pop()
            elif mode == 'duplicate': candidate['assets'].append(copy.deepcopy(candidate['assets'][0]))
            elif mode == 'empty': candidate['assets'][0]['size'] = 0
            elif mode == 'pending': candidate['assets'][0]['state'] = 'new'
            elif mode == 'foreign': candidate['assets'][0]['browser_download_url'] = 'https://other.invalid/install.sh'
            else:
                for asset in candidate['assets']: asset['name'] = asset['name'].replace('gofin-', 'fin-')
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, 'No completed GoFin beta'):
                site.select_beta([candidate])

    def test_no_ready_beta_refuses_without_creating_artifact(self):
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary) / 'site'
            with self.assertRaises(ValueError):
                site.build_site([], REPO / 'scripts/install.sh', out)
            self.assertFalse(out.exists())

    def test_generated_site_contains_only_pinned_installer_and_public_instructions(self):
        metadata = release()
        metadata['private_test_secret'] = 'NEVER-COPY-RELEASE-METADATA'
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary) / 'site'
            tag = site.build_site([metadata], REPO / 'scripts/install.sh', out)
            self.assertEqual(set(p.name for p in out.iterdir()), {'install.sh', 'index.html', '.nojekyll'})
            script = (out / 'install.sh').read_text()
            self.assertEqual(script, (REPO / 'scripts/install.sh').read_text().replace(site.DEFAULT_MARKER, f"gofin_default_version='{tag}'"))
            page = (out / 'index.html').read_text()
            self.assertIn('curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh', page)
            self.assertTrue(all('NEVER-COPY-RELEASE-METADATA' not in p.read_text() for p in out.iterdir()))
            with self.assertRaises(FileExistsError):
                site.build_site([metadata], REPO / 'scripts/install.sh', out)

    def test_unsafe_site_urls_and_ambiguous_installer_templates_refuse_before_writing(self):
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary) / 'site'
            for url in ['http://example.com', 'https://token@example.com', 'https://example.com?token=x', 'https://example.com/a;id', 'https://example.com/$(id)']:
                with self.subTest(url=url), self.assertRaises(ValueError):
                    site.build_site([release()], REPO / 'scripts/install.sh', out, url)
            installer = Path(temporary) / 'install.sh'
            for source in ['missing', site.DEFAULT_MARKER * 2]:
                installer.write_text(source)
                with self.assertRaises(ValueError):
                    site.build_site([release()], installer, out)
            self.assertFalse(out.exists())

    def test_actual_publish_guard_never_deploys_private_or_unapproved_previews(self):
        workflow = (REPO / '.github/workflows/installer-pages.yml').read_text()
        job = workflow.split('\n  publish:\n', 1)[1]
        guard = re.search(r'^    if: (.+)$', job, re.M)[1]
        for private in [True, False]:
            for enabled in ['true', 'false', '']:
                for event in ['workflow_dispatch', 'workflow_run']:
                    for publish in [True, False]:
                        expression = guard
                        for key, value in {'github.event.repository.private': private,
                                           'github.ref_name': 'main',
                                           'github.event.repository.default_branch': 'main',
                                           'vars.GOFIN_PAGES_INSTALLER_ENABLED': enabled,
                                           'github.event_name': event, 'inputs.publish': publish}.items():
                            expression = expression.replace(key, repr(value))
                        expression = re.sub(r'\btrue\b', 'True', expression)
                        expression = re.sub(r'\bfalse\b', 'False', expression)
                        expression = expression.replace('&&', ' and ').replace('||', ' or ')
                        tree = ast.parse(expression, mode='eval')
                        self.assertTrue(all(isinstance(node, (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.Compare, ast.Eq, ast.Constant)) for node in ast.walk(tree)))
                        actual = eval(compile(tree, '<publication guard>', 'eval'), {'__builtins__': {}})
                        expected = not private and enabled == 'true' and (event == 'workflow_run' or publish)
                        self.assertEqual(actual, expected, (private, enabled, event, publish))
                        # Even an otherwise authorized preview branch cannot publish.
                        if actual:
                            branch_guard = guard.replace('github.ref_name', repr('feature'))
                            for key, value in {'github.event.repository.private': False,
                                               'github.event.repository.default_branch': 'main',
                                               'vars.GOFIN_PAGES_INSTALLER_ENABLED': 'true',
                                               'github.event_name': event, 'inputs.publish': publish}.items():
                                branch_guard = branch_guard.replace(key, repr(value))
                            branch_guard = re.sub(r'\btrue\b', 'True', branch_guard)
                            branch_guard = re.sub(r'\bfalse\b', 'False', branch_guard)
                            branch_guard = branch_guard.replace('&&', ' and ').replace('||', ' or ')
                            self.assertFalse(eval(branch_guard, {'__builtins__': {}}))


if __name__ == '__main__':
    unittest.main()
