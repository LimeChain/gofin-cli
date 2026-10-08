#!/bin/sh
# Developer conveniences only. Setup/uninstall retain the real protected path.
set -eu
fin_action=${1:-}
fin_verbose=0
case "$fin_action" in bundle|release|setup|update|uninstall) ;; *) echo 'Usage: dev-macos.sh bundle|release|setup|update|uninstall [--verbose]' >&2; exit 2 ;; esac
shift
if [ "$#" -gt 0 ]; then
  [ "$#" = 1 ] && [ "$1" = --verbose ] && [ "$fin_action" != uninstall ] || {
    echo 'Usage: dev-macos.sh bundle|release|setup|update|uninstall [--verbose]' >&2
    exit 2
  }
  fin_verbose=1
fi
fin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
cd "$fin_source"
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'This target requires macOS; use make dev or make test-ui here.' >&2; exit 1; }
if [ "$fin_action" = setup ] || [ "$fin_action" = update ] || [ "$fin_action" = uninstall ]; then
  [ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Run this target yourself in macOS Terminal, without redirection.' >&2; exit 1; }
  [ "$(/usr/bin/id -u)" -ge 501 ] || { echo 'Run make as your own user, without sudo.' >&2; exit 1; }
  # Clear cached elevation after success, failure, or cancellation.
  trap '/usr/bin/sudo -K' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
fi
if [ "$fin_action" = uninstall ]; then
  printf '%s\n' 'Fin needs your Mac login password to remove its files, credentials, settings and financial history, including pending recovery records. Uninstall does not cancel existing transactions or orders on exchanges or chains.' >&2
  /usr/bin/sudo -k -p 'Mac login password for %p: ' -- /usr/bin/python3 -I "$fin_source/scripts/uninstall-rust-macos.py"
  exit 0
fi
if [ "$fin_action" = setup ]; then
  for fin_existing in /Library/Fin /Library/LaunchDaemons/io.fin-cli.broker.plist /private/etc/paths.d/fin; do
    [ ! -e "$fin_existing" ] && [ ! -L "$fin_existing" ] || { echo 'Fin is already installed. Setup preserves existing state; use the reviewed update path or make uninstall for an intentional reset.' >&2; exit 1; }
  done
fi
fin_runtime="/Library/Fin/runtime"
if [ "$fin_action" = update ]; then
  [ -x "$fin_runtime/fin" ] && [ ! -L "$fin_runtime" ] || {
    echo 'Fin is not installed. Run make setup first.' >&2; exit 1;
  }
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

if [ "$fin_action" = update ]; then
  # Status is a local service observation; do not inspect credentials or the journal.
  fin_observation=$("$fin_runtime/fin" daemon status --json)
  fin_previous_state=$(printf '%s' "$fin_observation" | /usr/bin/python3 -c '
import json,sys
try:
    value=json.load(sys.stdin)
except (ValueError, TypeError):
    sys.exit("Could not verify the installed Fin service. Check fin daemon status.")
if not isinstance(value, dict) or value.get("registered") is not True or value.get("status") not in ("running", "stopped"):
    sys.exit("Could not verify the installed Fin service. Check fin daemon status.")
print(value["status"])
')
  # Quiesce the old image. Stop preserves pending operations and durable state.
  "$fin_runtime/fin" daemon stop
  printf '%s\n' 'Fin needs your Mac login password to update its protected runtime, including the Touch ID helper. Saved connections are kept.' >&2
  if /usr/bin/sudo -k -p 'Mac login password for %p: ' -- /bin/bash "$fin_source/scripts/update-rust-macos-runtime.sh" \
      --from "$fin_runtime" --to "$fin_bundle" --presentation-update; then
    /usr/bin/sudo -K
  else
    fin_update_status=$?
    printf '%s\n' 'Update did not complete. Fin remains stopped; inspect the update error before restarting.' >&2
    exit "$fin_update_status"
  fi
  if [ "$fin_previous_state" = running ]; then
    "$fin_runtime/fin" daemon start
  else
    "$fin_runtime/fin" daemon status
  fi
fi
