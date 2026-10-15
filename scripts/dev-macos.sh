#!/bin/sh
# Developer conveniences only. Setup/uninstall retain the real protected path.
set -eu
gofin_action=${1:-}
gofin_verbose=0
case "$gofin_action" in bundle|release|setup|update|uninstall) ;; *) echo 'Usage: dev-macos.sh bundle|release|setup|update|uninstall [--verbose]' >&2; exit 2 ;; esac
shift
if [ "$#" -gt 0 ]; then
  [ "$#" = 1 ] && [ "$1" = --verbose ] && [ "$gofin_action" != uninstall ] || {
    echo 'Usage: dev-macos.sh bundle|release|setup|update|uninstall [--verbose]' >&2
    exit 2
  }
  gofin_verbose=1
fi
gofin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
cd "$gofin_source"
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'This target requires macOS; use make dev or make test-ui here.' >&2; exit 1; }
if [ "$gofin_action" = setup ] || [ "$gofin_action" = update ] || [ "$gofin_action" = uninstall ]; then
  [ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Run this target yourself in macOS Terminal, without redirection.' >&2; exit 1; }
  [ "$(/usr/bin/id -u)" -ge 501 ] || { echo 'Run make as your own user, without sudo.' >&2; exit 1; }
  # Clear cached elevation after success, failure, or cancellation.
  trap '/usr/bin/sudo -K' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
fi
if [ "$gofin_action" = uninstall ]; then
  printf '%s\n' 'GoFin needs your Mac login password to remove its files, credentials, settings and financial history, including pending recovery records. Uninstall does not cancel existing transactions or orders on exchanges or chains.' >&2
  /usr/bin/sudo -k -p 'Mac login password for %p: ' -- /usr/bin/python3 -I "$gofin_source/scripts/uninstall-rust-macos.py"
  exit 0
fi
if [ "$gofin_action" = setup ]; then
  for gofin_existing in /Library/Fin /Library/LaunchDaemons/io.fin-cli.broker.plist /private/etc/paths.d/fin; do
    [ ! -e "$gofin_existing" ] && [ ! -L "$gofin_existing" ] || { echo 'GoFin is already installed. Setup preserves existing state; use the reviewed update path or make uninstall for an intentional reset.' >&2; exit 1; }
  done
fi
gofin_runtime="/Library/Fin/runtime"
if [ "$gofin_action" = update ]; then
  gofin_installed_cli="$gofin_runtime/gofin"
  if [ ! -e "$gofin_installed_cli" ] && [ ! -L "$gofin_installed_cli" ]; then
    gofin_installed_cli="$gofin_runtime/fin"
  fi
  [ -x "$gofin_installed_cli" ] && [ ! -L "$gofin_installed_cli" ] && [ ! -L "$gofin_runtime" ] || {
    echo 'GoFin is not installed. Run make setup first.' >&2; exit 1;
  }
fi
mkdir -p "$gofin_source/work/dev-bundles"
gofin_directory=$(/usr/bin/mktemp -d "$gofin_source/work/dev-bundles/build.XXXXXX")
gofin_bundle="$gofin_directory/bundle"
if [ "$gofin_action" = release ]; then
  if [ "$gofin_verbose" = 1 ]; then /bin/sh scripts/package-rust-macos.sh "$gofin_bundle" --verbose
  else /bin/sh scripts/package-rust-macos.sh "$gofin_bundle"
  fi
else
  if [ "$gofin_verbose" = 1 ]; then /bin/sh scripts/package-rust-macos.sh "$gofin_bundle" --dev --verbose
  else /bin/sh scripts/package-rust-macos.sh "$gofin_bundle" --dev
  fi
fi
if [ "$gofin_action" = setup ]; then
  if [ "$gofin_verbose" = 1 ]; then "$gofin_bundle/gofin" setup --verbose
  else "$gofin_bundle/gofin" setup
  fi
fi

if [ "$gofin_action" = update ]; then
  # Status is a local service observation; do not inspect credentials or the journal.
  gofin_observation=$("$gofin_installed_cli" daemon status --json)
  gofin_previous_state=$(printf '%s' "$gofin_observation" | /usr/bin/python3 -c '
import json,sys
try:
    value=json.load(sys.stdin)
except (ValueError, TypeError):
    sys.exit("Could not verify the installed GoFin service. Check gofin daemon status.")
if not isinstance(value, dict) or value.get("registered") is not True or value.get("status") not in ("running", "stopped"):
    sys.exit("Could not verify the installed GoFin service. Check gofin daemon status.")
print(value["status"])
')
  # Quiesce the old image. Stop preserves pending operations and durable state.
  "$gofin_installed_cli" daemon stop
  printf '%s\n' 'GoFin needs your Mac login password to update its protected runtime, including the Touch ID helper. Saved connections are kept.' >&2
  if /usr/bin/sudo -k -p 'Mac login password for %p: ' -- /bin/bash "$gofin_source/scripts/update-rust-macos-runtime.sh" \
      --from "$gofin_runtime" --to "$gofin_bundle" --presentation-update; then
    /usr/bin/sudo -K
  else
    gofin_update_status=$?
    printf '%s\n' 'Update did not complete. GoFin remains stopped; inspect the update error before restarting.' >&2
    exit "$gofin_update_status"
  fi
  if [ "$gofin_previous_state" = running ]; then
    "$gofin_runtime/gofin" daemon start
  else
    "$gofin_runtime/gofin" daemon status
  fi
fi
