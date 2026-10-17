#!/usr/bin/env python3
"""Offline prerelease selection and static publication artifact checks."""
import copy
import ast
import importlib.util
import json
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
    def test_completed_rc2_is_selected_and_pinned_for_installer_and_feed(self):
        beta = release()
        rc = release('v0.3.0-rc2', '2026-01-03T00:00:00Z')
        for releases in [[beta, rc], [rc, beta]]:
            with self.subTest(order=[item['tag_name'] for item in releases]):
                self.assertEqual(site.select_prerelease(releases), rc['tag_name'])
                with tempfile.TemporaryDirectory() as temporary:
                    out = Path(temporary) / 'site'
                    self.assertEqual(site.build_site(releases, REPO / 'scripts/install.sh', out), rc['tag_name'])
                    self.assertEqual(json.loads((out / 'latest-beta.json').read_text()),
                                     {'schema_version': 1, 'tag': rc['tag_name']})
                    self.assertIn("gofin_default_version='v0.3.0-rc2'", (out / 'install.sh').read_text())

    def test_invalid_or_unfinished_rc_does_not_replace_a_completed_beta(self):
        ready = release()
        for tag in ['v0.3.0-rc01', 'v0.3.0-rc.01', 'v0.3.0-rcandidate',
                    'v0.3.0-rc--2', 'v0.3.0-rc2+build', "v0.3.0-rc2';exit"]:
            with self.subTest(tag=tag):
                self.assertEqual(site.select_prerelease([release(tag, '2026-01-03T00:00:00Z'), ready]), ready['tag_name'])
        for mutation in [dict(draft=True), dict(prerelease=False), dict(assets=[])]:
            candidate = release('v0.3.0-rc2', '2026-01-03T00:00:00Z')
            candidate.update(mutation)
            with self.subTest(mutation=mutation):
                self.assertEqual(site.select_prerelease([candidate, ready]), ready['tag_name'])

    def test_canonical_rc_tag_formats_are_supported(self):
        for tag in ['v0.3.0-rc', 'v0.3.0-rc0', 'v0.3.0-rc2', 'v0.3.0-rc.2']:
            with self.subTest(tag=tag):
                self.assertEqual(site.select_prerelease([release(tag)]), tag)

    def test_newest_completed_beta_wins_regardless_of_api_order(self):
        old = release()
        new = release('v0.2.0-beta2', '2026-01-03T00:00:00Z')
        for releases in [[new, old], [old, new]]:
            self.assertEqual(site.select_prerelease(releases), new['tag_name'])

    def test_drafts_stable_alpha_and_incomplete_releases_do_not_replace_ready_beta(self):
        ready = release()
        for mutation in [dict(draft=True), dict(prerelease=False), dict(tag_name='v0.3.0'),
                         dict(tag_name='v0.3.0-alpha.1'), dict(tag_name='v0.3.0-betamax'),
                         dict(tag_name='v0.3.0-beta.01'), dict(tag_name="v0.3.0-beta';exit"),
                         dict(assets=[]), dict(published_at=None), dict(published_at='bad')]:
            candidate = release('v0.3.0-beta.1', '2026-01-03T00:00:00Z')
            candidate.update(mutation)
            with self.subTest(mutation=mutation):
                self.assertEqual(site.select_prerelease([candidate, ready]), ready['tag_name'])

    def test_missing_duplicate_empty_pending_and_foreign_assets_are_not_ready(self):
        for tag in ['v0.2.0-beta.1', 'v0.3.0-rc2']:
            for mode in ['missing', 'duplicate', 'empty', 'pending', 'foreign', 'old-fin-names']:
                candidate = release(tag)
                if mode == 'missing': candidate['assets'].pop()
                elif mode == 'duplicate': candidate['assets'].append(copy.deepcopy(candidate['assets'][0]))
                elif mode == 'empty': candidate['assets'][0]['size'] = 0
                elif mode == 'pending': candidate['assets'][0]['state'] = 'new'
                elif mode == 'foreign': candidate['assets'][0]['browser_download_url'] = 'https://other.invalid/install.sh'
                else:
                    for asset in candidate['assets']: asset['name'] = asset['name'].replace('gofin-', 'fin-')
                with self.subTest(tag=tag, mode=mode), self.assertRaisesRegex(ValueError, 'No completed GoFin beta or RC'):
                    site.select_prerelease([candidate])

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
            self.assertEqual(set(p.name for p in out.iterdir()), {'install.sh', 'latest-beta.json', 'index.html', '.nojekyll'})
            script = (out / 'install.sh').read_text()
            self.assertEqual(script, (REPO / 'scripts/install.sh').read_text().replace(site.DEFAULT_MARKER, f"gofin_default_version='{tag}'"))
            self.assertEqual(json.loads((out / 'latest-beta.json').read_text()), {'schema_version': 1, 'tag': tag})
            page = (out / 'index.html').read_text()
            self.assertIn('curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh', page)
            self.assertTrue(all('NEVER-COPY-RELEASE-METADATA' not in p.read_text() for p in out.iterdir()))
            with self.assertRaises(FileExistsError):
                site.build_site([metadata], REPO / 'scripts/install.sh', out)

    def test_update_feed_and_installer_select_the_same_completed_beta(self):
        ready = release('v0.2.0-beta.2', '2026-01-02T00:00:00Z')
        unfinished = release('v0.3.0-beta.1', '2026-01-03T00:00:00Z')
        unfinished['assets'].pop()
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary) / 'site'
            tag = site.build_site([unfinished, release(), ready], REPO / 'scripts/install.sh', out)
            feed = json.loads((out / 'latest-beta.json').read_text())
            self.assertEqual(feed, {'schema_version': 1, 'tag': ready['tag_name']})
            self.assertEqual(tag, feed['tag'])
            self.assertIn(f"gofin_default_version='{feed['tag']}'", (out / 'install.sh').read_text())

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
        for ref in ['refs/heads/main', 'refs/heads/feature', 'refs/tags/main', 'refs/tags/v1.0.0']:
            for private in [True, False]:
                for enabled in ['true', 'false', '']:
                    for event in ['workflow_dispatch', 'workflow_run']:
                        for publish in [True, False]:
                            context = {'github.event.repository.private': private,
                                       'github.ref': ref, 'github.event_name': event,
                                       'vars.GOFIN_PAGES_INSTALLER_ENABLED': enabled,
                                       'inputs.publish': publish}
                            expression = guard.replace("format('refs/heads/{0}', github.event.repository.default_branch)", repr('refs/heads/main'))
                            # Preserve quoted strings; replace only expression tokens.
                            def token(match):
                                value = match.group()
                                if value in context:
                                    return repr(context[value])
                                return {'true': 'True', 'false': 'False'}.get(value, value)
                            expression = re.sub(r"'[^']*'|[A-Za-z_][A-Za-z0-9_.]*", token, expression)
                            expression = expression.replace('&&', ' and ').replace('||', ' or ')
                            tree = ast.parse(expression, mode='eval')
                            self.assertTrue(all(isinstance(node, (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.Compare, ast.Eq, ast.Constant)) for node in ast.walk(tree)))
                            actual = eval(compile(tree, '<publication guard>', 'eval'), {'__builtins__': {}})
                            expected = ref == 'refs/heads/main' and not private and enabled == 'true' and (event == 'workflow_run' or publish)
                            self.assertEqual(actual, expected, (ref, private, enabled, event, publish))


if __name__ == '__main__':
    unittest.main()
