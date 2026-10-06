#!/bin/sh
# Developer conveniences only. Setup/uninstall retain the real protected path.
set -eu
fin_action=${1:-}
fin_verbose=0
case "$fin_action" in bundle|release|setup|uninstall) ;; *) echo 'Usage: dev-macos.sh bundle|release|setup|uninstall [--verbose]' >&2; exit 2 ;; esac
shift
if [ "$#" -gt 0 ]; then
  [ "$#" = 1 ] && [ "$1" = --verbose ] && [ "$fin_action" != uninstall ] || {
    echo 'Usage: dev-macos.sh bundle|release|setup|uninstall [--verbose]' >&2
    exit 2
  }
  fin_verbose=1
fi
fin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
cd "$fin_source"
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'This target requires macOS; use make dev or make test-ui here.' >&2; exit 1; }
if [ "$fin_action" = setup ] || [ "$fin_action" = uninstall ]; then
  [ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Run this target yourself in macOS Terminal, without redirection.' >&2; exit 1; }
  [ "$(/usr/bin/id -u)" -ge 501 ] || { echo 'Run make as your own user, without sudo.' >&2; exit 1; }
  # Clear cached elevation after success, failure, or cancellation.
  trap '/usr/bin/sudo -K' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
fi
if [ "$fin_action" = uninstall ]; then
  printf '%s\n' 'Fin needs your Mac login password to delete its installed credentials and configuration.' >&2
  /usr/bin/sudo -k -p 'Mac login password for %p: ' -- /usr/bin/python3 -I "$fin_source/scripts/uninstall-rust-macos.py"
  exit 0
fi
if [ "$fin_action" = setup ]; then
  for fin_existing in /Library/Fin /Library/LaunchDaemons/io.fin-cli.broker.plist /private/etc/paths.d/fin; do
    [ ! -e "$fin_existing" ] && [ ! -L "$fin_existing" ] || { echo 'Fin is already installed. Setup preserves existing state; use the reviewed update path or make uninstall for an intentional reset.' >&2; exit 1; }
  done
fi
mkdir -p "$fin_source/work/dev-bundles"
fin_directory=$(/usr/bin/mktemp -d "$fin_source/work/dev-bundles/build.XXXXXX")
fin_bundle="$fin_directory/bundle"
if [ "$fin_action" = release ]; then
  if [ "$fin_verbose" = 1 ]; then /bin/sh scripts/package-rust-macos.sh "$fin_bundle" --verbose
  else /bin/sh scripts/package-rust-macos.sh "$fin_bundle"
  fi
else
  if [ "$fin_verbose" = 1 ]; then /bin/sh scripts/package-rust-macos.sh "$fin_bundle" --dev --verbose
  else /bin/sh scripts/package-rust-macos.sh "$fin_bundle" --dev
  fi
fi
if [ "$fin_action" = setup ]; then
  if [ "$fin_verbose" = 1 ]; then "$fin_bundle/fin" setup --verbose
  else "$fin_bundle/fin" setup
  fi
fi
