#!/usr/bin/env python3
"""Synthetic published-release uploads; no network or actual GitHub writes."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('fin_upload', Path(__file__).with_name('upload-release.py'))
uploader = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(uploader)


class UploadTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='fin-upload-test-')
        self.addCleanup(temporary.cleanup)
        self.assets = Path(temporary.name)
        self.tag = 'v0.2.0-beta.1'
        self.repo = 'synthetic/fin'
        self.files = {uploader.release_build.filename(self.tag, target): b'synthetic archive ' + target.encode()
                      for target in uploader.release_build.TARGETS}
        self.files['install.sh'] = b'#!/bin/sh\n# synthetic fixture\n'
        for name, data in self.files.items():
            (self.assets/name).write_bytes(data)
        (self.assets/'SHA256SUMS').write_bytes(uploader.release_build.manifest(self.files))
        self.files['SHA256SUMS'] = (self.assets/'SHA256SUMS').read_bytes()
        self.remote = {'id':123, 'tag_name':self.tag, 'draft':False, 'assets':[]}
        self.commands = []
        self.fail_after = None

    def asset(self, name, digest=True, data=None):
        data = self.files[name] if data is None else data
        return {'name':name, 'state':'uploaded', 'digest':'sha256:' + hashlib.sha256(data).hexdigest() if digest else None}

    def command(self, args):
        self.commands.append(args)
        if args[1] == 'api':
            return json.dumps(self.remote)
        if args[2] == 'upload':
            if self.fail_after is not None and len(self.remote['assets']) >= self.fail_after:
                raise subprocess.CalledProcessError(1, args)
            name = Path(args[4]).name
            self.remote['assets'].append(self.asset(name))
        elif args[2] == 'download':
            name=args[args.index('--pattern')+1]
            (Path(args[args.index('--dir')+1])/name).write_bytes(self.files[name])
        else:
            self.fail('unexpected command: ' + repr(args))
        return ''

    def upload(self):
        return uploader.upload(self.repo,self.tag,123,self.assets,self.command)

    def assert_no_upload(self):
        self.assertFalse(any(cmd[1:3] == ['release','upload'] for cmd in self.commands))

    def test_existing_published_release_receives_assets_manifest_last(self):
        self.assertEqual(self.upload(),list(self.files))
        self.assertEqual(self.remote['assets'][-1]['name'],'SHA256SUMS')
        self.assertEqual(self.remote['tag_name'],self.tag)
        self.assertFalse(self.remote['draft'])
        self.assertTrue(all('--clobber' not in args for args in self.commands))

    def test_retry_matching_assets_is_noop_and_never_replaces(self):
        self.upload(); self.commands.clear()
        self.assertEqual(self.upload(),[])
        self.assert_no_upload()

    def test_partial_failure_resumes_without_duplicate_uploads(self):
        self.fail_after=2
        with self.assertRaises(subprocess.CalledProcessError): self.upload()
        self.assertEqual(len(self.remote['assets']),2)
        self.fail_after=None; self.commands.clear()
        self.assertEqual(self.upload(),list(self.files)[2:])
        self.assertEqual(len(self.remote['assets']),len(self.files))

    def test_draft_wrong_id_or_tag_and_api_failure_refuse(self):
        for values in ({'draft':True},{'id':124},{'tag_name':'v0.2.0-other'}):
            with self.subTest(values=values):
                original=dict(self.remote); self.remote.update(values)
                with self.assertRaises(ValueError): self.upload()
                self.assert_no_upload(); self.remote=original
        def failure(args): raise subprocess.CalledProcessError(1,args)
        with self.assertRaises(subprocess.CalledProcessError):
            uploader.upload(self.repo,self.tag,123,self.assets,failure)
        self.assert_no_upload()

    def test_collision_and_incomplete_asset_refuse_all_new_uploads(self):
        for asset in (self.asset('install.sh',data=b'different'),dict(self.asset('install.sh'),state='starter')):
            with self.subTest(asset=asset):
                self.remote['assets']=[asset]
                with self.assertRaises(ValueError): self.upload()
                self.assert_no_upload()

    def test_missing_remote_digest_verifies_downloaded_bytes(self):
        self.remote['assets']=[self.asset('install.sh',digest=False)]
        self.assertNotIn('install.sh',self.upload())
        self.assertTrue(any(cmd[1:3] == ['release','download'] for cmd in self.commands))

    def test_corrupt_manifest_blocks_network_and_uploads(self):
        (self.assets/'SHA256SUMS').write_bytes(b'changed')
        with self.assertRaises(ValueError): self.upload()
        self.assertEqual(self.commands,[])

    def test_workflow_only_packages_published_release_event(self):
        workflow=(Path(__file__).resolve().parent.parent/'.github/workflows/release.yml').read_text()
        trigger=workflow.split('on:\n',1)[1].split('\npermissions:',1)[0]
        self.assertIn('release:\n    types: [published]',trigger)
        self.assertNotIn('push:',trigger)
        self.assertNotIn('workflow_dispatch:',trigger)
        self.assertIn('${{ github.event.release.tag_name }}',workflow)
        self.assertIn('${{ github.event.release.id }}',workflow)
        self.assertNotIn("'release', 'create'",workflow)
        self.assertIn('scripts/upload-release.py',workflow)


if __name__ == '__main__':
    unittest.main()
