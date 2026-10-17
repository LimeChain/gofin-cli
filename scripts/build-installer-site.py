#!/usr/bin/env python3
"""Build a static prerelease installer site from release metadata; never publish it."""
import argparse
from datetime import datetime, timezone
import html
import json
from pathlib import Path
import re
from urllib.parse import urlsplit

REPOSITORY = 'LimeChain/gofin-cli'
SITE_URL = 'https://limechain.github.io/gofin-cli'
DEFAULT_MARKER = "gofin_default_version=''"
VERSION = re.compile(r'^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-((?:beta|rc)(?:0|[1-9][0-9]*)?)(?:\.[0-9A-Za-z-]+)*$')
TARGETS = ('aarch64-apple-darwin', 'x86_64-apple-darwin', 'x86_64-unknown-linux-gnu-discovery')


def select_prerelease(releases):
    """Newest publication with every GoFin delivery asset fully uploaded wins."""
    if not isinstance(releases, list):
        raise ValueError('Expected a JSON array of GitHub releases.')
    eligible = []
    for release in releases:
        if not isinstance(release, dict):
            continue
        tag = release.get('tag_name', '')
        if release.get('draft') is not False or release.get('prerelease') is not True or not isinstance(tag, str) or not VERSION.fullmatch(tag):
            continue
        # Match the installer's SemVer numeric prerelease rules too.
        if any(part.isdigit() and len(part) > 1 and part.startswith('0') for part in tag.split('-', 1)[1].split('.')):
            continue
        try:
            timestamp = release['published_at']
            if not isinstance(timestamp, str):
                continue
            published = datetime.fromisoformat(timestamp.replace('Z', '+00:00'))
            if published.utcoffset() != timezone.utc.utcoffset(published):
                continue
        except (KeyError, TypeError, ValueError):
            continue
        required = {'install.sh', 'SHA256SUMS'} | {f'gofin-{tag}-{target}.tar.gz' for target in TARGETS}
        assets = release.get('assets', [])
        if not isinstance(assets, list):
            continue
        names = [a.get('name') for a in assets if isinstance(a, dict)]
        if any(names.count(name) != 1 for name in required):
            continue
        base = f'https://github.com/{REPOSITORY}/releases/download/{tag}/'
        selected = [a for a in assets if isinstance(a, dict) and a.get('name') in required]
        if not all(a.get('state') == 'uploaded' and type(a.get('size')) is int and a['size'] > 0
                   and a.get('browser_download_url') == base + a['name'] for a in selected):
            continue
        eligible.append((published, tag))
    if not eligible:
        raise ValueError('No completed GoFin beta or RC release is available; publish its installer, all bundles and SHA256SUMS first.')
    return max(eligible)[1]


def build_site(releases, installer, output, site_url=SITE_URL):
    url = urlsplit(site_url)
    if url.scheme != 'https' or not url.hostname or url.username or url.password or url.query or url.fragment or not re.fullmatch(r'https://[A-Za-z0-9.-]+(?::[0-9]+)?(?:/[A-Za-z0-9._~/-]*)?', site_url):
        raise ValueError('The site URL must be an HTTPS URL without credentials, query or fragment.')
    tag = select_prerelease(releases)
    source = installer.read_text()
    if source.count(DEFAULT_MARKER) != 1:
        raise ValueError('Installer default marker is missing or ambiguous.')
    pinned = source.replace(DEFAULT_MARKER, f"gofin_default_version='{tag}'")
    # Require a new path: never overwrite a prior artifact or follow a symlink.
    output.mkdir(mode=0o700)
    (output / 'install.sh').write_text(pinned)
    # A tiny public feed shares the exact completed release selected for install.sh.
    (output / 'latest-beta.json').write_text(json.dumps({'schema_version': 1, 'tag': tag}, separators=(',', ':')) + '\n')
    (output / '.nojekyll').write_text('')
    command = f'curl -fsSL {site_url.rstrip("/")}/install.sh | sh'
    (output / 'index.html').write_text(
        '<!doctype html><html lang="en"><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width,initial-scale=1">'
        '<title>Install GoFin prerelease</title><h1>Install GoFin prerelease</h1>'
        '<p>Run in your own macOS Terminal:</p><pre>' + html.escape(command) + '</pre>'
        f'<p>Current prerelease: <a href="https://github.com/{REPOSITORY}/releases/tag/{tag}">{tag}</a>.</p>'
        '<p>The installer verifies checksums and launches protected setup. '
        'Existing installations are preserved; use <code>gofin update</code> in your macOS Terminal to upgrade them.</p></html>\n')
    return tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--releases-json', required=True, type=Path)
    parser.add_argument('--installer', type=Path, default=Path(__file__).resolve().parent / 'install.sh')
    parser.add_argument('--output-dir', required=True, type=Path)
    parser.add_argument('--site-url', default=SITE_URL)
    args = parser.parse_args()
    try:
        tag = build_site(json.loads(args.releases_json.read_text()), args.installer, args.output_dir, args.site_url)
    except (OSError, ValueError, TypeError) as error:
        parser.exit(1, f'Installer site was not built: {error}\n')
    print(f'Prepared GoFin prerelease installer for {tag}. Nothing was published.')


if __name__ == '__main__':
    main()
