#!/usr/bin/env python3
"""Attach verified assets to an existing published release; never replace bytes."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

SPEC = importlib.util.spec_from_file_location('gofin_release', Path(__file__).with_name('build-release.py'))
release_build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_build)


def run(args):
    return subprocess.check_output(args, text=True)


def upload(repo, tag, release_id, assets, command=run):
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repo) or release_id <= 0:
        raise ValueError('invalid repository or release identity')
    if not re.fullmatch('v' + release_build.VERSION, tag):
        raise ValueError('invalid version tag')
    names = [release_build.filename(tag, target) for target in release_build.TARGETS] + ['install.sh']
    files = {name: (assets/name).read_bytes() for name in names}
    release_build.verify_manifest((assets/'SHA256SUMS').read_bytes(), files)
    files['SHA256SUMS'] = (assets/'SHA256SUMS').read_bytes()
    current = json.loads(command(['gh', 'api', 'repos/' + repo + '/releases/' + str(release_id)]))
    if current['id'] != release_id or current['tag_name'] != tag or current['draft']:
        raise ValueError('the triggering published release no longer matches; no assets changed')
    existing = {}
    for asset in current['assets']:
        if asset['name'] in existing:
            raise ValueError('release has duplicate asset names; no assets changed')
        existing[asset['name']] = asset
    pending = []
    # Check every collision before uploading anything. Successful partial uploads
    # can resume, but changed published bytes are never silently replaced.
    for name, data in files.items():
        found = existing.get(name)
        if found is None:
            pending.append(name)
            continue
        if found.get('state') != 'uploaded':
            raise ValueError('an existing asset is incomplete; inspect it before retrying')
        expected = 'sha256:' + hashlib.sha256(data).hexdigest()
        actual = found.get('digest')
        if not actual:
            with tempfile.TemporaryDirectory(prefix='gofin-release-check-') as folder:
                command(['gh', 'release', 'download', tag, '--repo', repo, '--pattern', name, '--dir', folder])
                actual = 'sha256:' + hashlib.sha256((Path(folder)/name).read_bytes()).hexdigest()
        if actual != expected:
            raise ValueError('an existing asset differs; no published assets were replaced')
    # Publish the readiness manifest last: the installer refuses absent or
    # mismatched manifests, even while a release's uploads are still in progress.
    for name in pending:
        command(['gh', 'release', 'upload', tag, str(assets/name), '--repo', repo])
    return pending


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--release-id', type=int, required=True)
    parser.add_argument('--assets-dir', type=Path, required=True)
    args = parser.parse_args()
    names = upload(args.repo, args.tag, args.release_id, args.assets_dir)
    print('Release assets verified; {} uploaded.'.format(len(names)))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print('Release upload failed: {}'.format(error), file=sys.stderr)
        sys.exit(1)
