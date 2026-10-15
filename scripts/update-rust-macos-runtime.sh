#!/bin/bash
# Reviewed local-beta runtime update only: same API/config/store/financial
# behavior. Helper/installer changes require an explicit human-selected
# --presentation-update flag and still pass the same bundle checks.
# Human: stop issuing GoFin commands and finish/cancel enrollment first. Review
# this script, its companion schema checker, and both bundles, then run in your
# own fresh macOS Terminal:
#   sudo -k /bin/bash /absolute/path/to/this-script --from OLD_BUNDLE --to NEW_BUNDLE
#   sudo -K
# Only user-owned completion and companion-skill refresh run after runtime commit.
# Rename migrations reload launchd registration only after the daemon is stopped.
# No financial-record/credential access occurs. The updater
# reads only the protected SQLite schema-version metadata for compatibility.
# For updates without a rename, existing daemon processes keep their loaded
# image; enrollment starts the new broker directly. Process checks are advisory,
# not a synchronization guarantee.
# Per-file renames are atomic, not the entire bundle; ordinary partial failures
# restore changed files. A power loss/SIGKILL needs inspection using the retained
# runtime-only backup. Never rerun setup or erase private state to recover.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH

fail() { echo "$*" >&2; exit 1; }
usage() { echo 'Usage: sudo -k /bin/bash update-rust-macos-runtime.sh --from /reviewed/old-bundle --to /reviewed/new-bundle [--presentation-update]' >&2; exit 2; }
gofin_from=''
gofin_to=''
gofin_presentation_update=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --presentation-update) [ "$gofin_presentation_update" = 0 ] || usage; gofin_presentation_update=1; shift; continue ;;
    --from|--to)
      [ "$#" -ge 2 ] || usage
      case "$1" in
        --from) [ -z "$gofin_from" ] || usage; gofin_from=$2 ;;
        --to) [ -z "$gofin_to" ] || usage; gofin_to=$2 ;;
      esac
      shift 2
      ;;
    *) usage ;;
  esac
done
case "$gofin_from" in /*) ;; *) usage ;; esac
case "$gofin_to" in /*) ;; *) usage ;; esac
[ "$(/usr/bin/uname -s)" = Darwin ] || fail 'This runtime hotfix requires macOS.'
[ "$(/usr/bin/id -u)" = 0 ] || fail 'The human must run this reviewed script with fresh sudo -k in their own Terminal.'
case "${SUDO_UID:-}" in ''|*[!0-9]*) fail 'Run through human sudo, not an elevated shell.' ;; esac
[ "$SUDO_UID" -ge 501 ] || fail 'Run through human sudo, not an elevated shell.'
[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || fail 'Use a fresh human Terminal without pipelines or redirection.'

gofin_runtime=/Library/Fin/runtime
gofin_auth_dir=$gofin_runtime/authorization
gofin_auth_helper=$gofin_auth_dir/GoFin
gofin_old_name=gofin
gofin_old_display=GoFin
if [ -f "$gofin_from/fin" ] && [ ! -e "$gofin_from/gofin" ] && [ ! -L "$gofin_from/gofin" ]; then
  gofin_old_name=fin
  gofin_old_display=Fin
fi
gofin_old_auth_helper=$gofin_auth_dir/$gofin_old_display
gofin_old_files="$gofin_old_name $gofin_old_name-broker $gofin_old_name-touch-id install.sh README.md BUILDINFO SHA256SUMS"
gofin_plist=/Library/LaunchDaemons/io.fin-cli.broker.plist
gofin_path_file=/private/etc/paths.d/fin
gofin_plist_changed=0
gofin_service_initial=0
gofin_service_detached=0
old_component() {
  case "$1" in
    gofin) printf '%s' "$gofin_old_name" ;;
    gofin-broker) printf '%s-broker' "$gofin_old_name" ;;
    gofin-touch-id) printf '%s-touch-id' "$gofin_old_name" ;;
    *) printf '%s' "$1" ;;
  esac
}
gofin_files='gofin gofin-broker gofin-touch-id install.sh README.md BUILDINFO SHA256SUMS'
gofin_changed=''
gofin_backup=''
gofin_replacing=0
gofin_complete=0
gofin_locked=0
gofin_auth_initial=0
gofin_auth_changed=0
gofin_auth_dir_created=0
gofin_update_lock=/Library/Fin/.runtime-update.lock

check_protected_path() {
  [ ! -L "$1" ] && [ "$(/usr/bin/stat -f '%u' "$1")" = 0 ] || fail 'Installed runtime paths must be real and root-owned.'
  /bin/ls -ld "$1" | /usr/bin/awk '{if (substr($1,6,1)=="w" || substr($1,9,1)=="w" || index($1,"+")) bad=1} END {exit bad}' || fail 'Installed runtime paths must have no group/world write access or extended ACLs.'
}

check_bundle() {
  local gofin_base=$2 gofin_bundle_files="$2 $2-broker $2-touch-id install.sh README.md BUILDINFO SHA256SUMS"
  [ -d "$1" ] && [ ! -L "$1" ] || fail 'Bundle/runtime must be a real directory.'
  for gofin_component in $gofin_bundle_files; do
    [ -f "$1/$gofin_component" ] && [ ! -L "$1/$gofin_component" ] || fail 'Bundle/runtime contains a missing, nonregular, or symbolic-link component.'
  done
  for gofin_entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
    [ -e "$gofin_entry" ] || [ -L "$gofin_entry" ] || continue
    case "${gofin_entry##*/}" in
      "$gofin_base"|"$gofin_base-broker"|"$gofin_base-touch-id"|install.sh|README.md|BUILDINFO|SHA256SUMS) ;;
      authorization) [ "$1" = "$gofin_runtime" ] && [ -d "$gofin_entry" ] && [ ! -L "$gofin_entry" ] || fail 'This hotfix only supports the fixed seven-component beta bundle and its derived authorization helper.' ;;
      *) fail 'This hotfix only supports the fixed seven-component beta bundle.' ;;
    esac
  done
  /usr/bin/awk -v base="$gofin_base" 'NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ {bad=1}
    $2 != base && $2 != base "-broker" && $2 != base "-touch-id" && $2 != "install.sh" && $2 != "README.md" && $2 != "BUILDINFO" {bad=1}
    {if (++seen[$2] > 1) bad=1; count++} END {if (count != 6) bad=1; exit bad}' "$1/SHA256SUMS" || fail 'Bundle checksum manifest is invalid.'
  (cd "$1" && /usr/bin/shasum -a 256 -c SHA256SUMS >/dev/null) || fail 'Bundle/runtime checksum verification failed.'
}

check_code_path() {
  /usr/bin/codesign --verify --strict "$1" || fail 'Bundle code signature verification failed.'
  /usr/bin/codesign -d --verbose=4 "$1" 2>&1 | /usr/bin/awk '/^CodeDirectory / && /flags=0x[0-9a-f]+\([^)]*runtime[,)]/ {valid=1} END {exit !valid}' || fail 'Bundle lacks hardened signing.'
  /usr/bin/otool -L "$1" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}' || fail 'Bundle uses non-OS libraries.'
}

check_code() {
  for gofin_binary in "$2" "$2-broker" "$2-touch-id"; do
    check_code_path "$1/$gofin_binary"
  done
}

check_auth_helper() {
  local gofin_auth_helper=$gofin_auth_dir/$2
  [ -d "$gofin_auth_dir" ] && [ ! -L "$gofin_auth_dir" ] || fail 'Installed authorization helper directory is not a protected real directory.'
  [ -f "$gofin_auth_helper" ] && [ ! -L "$gofin_auth_helper" ] || fail 'Installed GoFin authorization helper is missing or not a regular file.'
  check_protected_path "$gofin_auth_dir"
  check_protected_path "$gofin_auth_helper"
  [ "$(/usr/bin/stat -f '%g' "$gofin_auth_dir")" = 0 ] || fail 'Installed authorization helper directory group is invalid.'
  [ "$(/usr/bin/stat -f '%g' "$gofin_auth_helper")" = 0 ] || fail 'Installed GoFin authorization helper group is invalid.'
  [ "$(/usr/bin/stat -f '%Lp' "$gofin_auth_dir")" = 755 ] || fail 'Installed authorization helper directory permissions are invalid.'
  [ "$(/usr/bin/stat -f '%Lp' "$gofin_auth_helper")" = 755 ] || fail 'Installed GoFin authorization helper permissions are invalid.'
  for gofin_entry in "$gofin_auth_dir"/* "$gofin_auth_dir"/.[!.]* "$gofin_auth_dir"/..?*; do
    [ -e "$gofin_entry" ] || [ -L "$gofin_entry" ] || continue
    [ "$gofin_entry" = "$gofin_auth_helper" ] || fail 'Installed authorization helper directory contains an unexpected component.'
  done
  /usr/bin/cmp -s "$gofin_auth_helper" "$gofin_runtime/$1-touch-id" || fail 'Installed GoFin authorization helper differs from its signed source component.'
  check_code_path "$gofin_auth_helper"
}

check_idle_clients() {
  # pgrep emits only PIDs (discarded), never other processes' arguments.
  local gofin_status=0
  /usr/bin/pgrep -x 'fin|fin-touch-id|Fin|gofin|gofin-touch-id|GoFin' >/dev/null || gofin_status=$?
  [ "$gofin_status" = 1 ] || fail 'A GoFin client/approval is active or process inspection failed; finish it and retry.'
  gofin_status=0
  /usr/bin/pgrep -f '(^|/)(fin|gofin)-broker[[:space:]]+enrollment([[:space:]]|$)' >/dev/null || gofin_status=$?
  [ "$gofin_status" = 1 ] || fail 'Enrollment is active or process inspection failed; finish/cancel it and retry.'
  if [ "$gofin_old_name" = fin ]; then
    gofin_status=0
    /usr/bin/pgrep -x 'fin-broker|gofin-broker' >/dev/null || gofin_status=$?
    [ "$gofin_status" = 1 ] || fail 'Stop the installed daemon with fin daemon stop before renaming its runtime.'
  fi
}

cleanup() {
  local gofin_exit=$?
  trap - EXIT HUP INT TERM
  if [ "$gofin_replacing" = 1 ] && [ "$gofin_complete" = 0 ]; then
    echo 'Update failed/interrupted; restoring changed runtime files from the verified backup.' >&2
    local gofin_restore_ok=1
    # Restore every old component, including names removed during a migration.
    # Mark replacement before any rename so a signal cannot bypass rollback.
    for gofin_component in $gofin_old_files; do
      if ! /bin/cp -p "$gofin_backup/previous/$gofin_component" "$gofin_backup/restore/$gofin_component" ||
         ! /bin/mv -f "$gofin_backup/restore/$gofin_component" "$gofin_runtime/$gofin_component"; then
        gofin_restore_ok=0
      fi
    done
    if [ "$gofin_old_name" = fin ]; then
      /bin/rm -f "$gofin_runtime/gofin" "$gofin_runtime/gofin-broker" "$gofin_runtime/gofin-touch-id" || gofin_restore_ok=0
    fi
    if [ "$gofin_auth_changed" = 1 ]; then
      if [ "$gofin_auth_initial" = 1 ]; then
        if ! /bin/cp -p "$gofin_backup/previous-GoFin" "$gofin_backup/restore-GoFin" ||
           ! /bin/mv -f "$gofin_backup/restore-GoFin" "$gofin_old_auth_helper"; then
          gofin_restore_ok=0
        fi
      else
        /bin/rm -f "$gofin_auth_helper" || gofin_restore_ok=0
      fi
      if [ "$gofin_old_name" = fin ]; then /bin/rm -f "$gofin_auth_helper" || gofin_restore_ok=0; fi
      if [ "$gofin_auth_dir_created" = 1 ] && [ -d "$gofin_auth_dir" ]; then
        /bin/rmdir "$gofin_auth_dir" 2>/dev/null || gofin_restore_ok=0
      fi
    fi
    if [ "$gofin_plist_changed" = 1 ]; then
      if ! /bin/cp -p "$gofin_backup/previous.plist" "$gofin_backup/restore.plist" ||
         ! /bin/mv -f "$gofin_backup/restore.plist" "$gofin_plist"; then gofin_restore_ok=0; fi
      /usr/bin/cmp -s "$gofin_backup/previous.plist" "$gofin_plist" || gofin_restore_ok=0
    fi
    if [ "$gofin_service_detached" = 1 ]; then
      /bin/launchctl bootout system/io.fin-cli.broker >/dev/null 2>&1 || true
      if [ "$gofin_service_initial" = 1 ]; then
        /bin/launchctl bootstrap system "$gofin_plist" || gofin_restore_ok=0
      fi
    fi
    for gofin_component in $gofin_old_files; do
      /usr/bin/cmp -s "$gofin_backup/previous/$gofin_component" "$gofin_runtime/$gofin_component" || gofin_restore_ok=0
    done
    if [ "$gofin_auth_initial" = 1 ]; then
      /usr/bin/cmp -s "$gofin_backup/previous-GoFin" "$gofin_old_auth_helper" || gofin_restore_ok=0
    else
      [ ! -e "$gofin_auth_dir" ] && [ ! -L "$gofin_auth_dir" ] || gofin_restore_ok=0
    fi
    if [ "$gofin_restore_ok" = 1 ]; then
      echo 'Previous runtime restored. Saved state was preserved.' >&2
    else
      echo 'Runtime restoration is uncertain. Stop issuing GoFin commands and inspect the retained runtime backup; do not rerun setup.' >&2
    fi
    gofin_exit=1
  fi
  if [ -n "$gofin_backup" ]; then
    if [ "$gofin_replacing" = 0 ]; then
      /bin/rm -rf "$gofin_backup"
    else
      echo "Runtime-only backup retained: $gofin_backup" >&2
    fi
  fi
  if [ "$gofin_locked" = 1 ]; then /bin/rmdir "$gofin_update_lock"; fi
  exit "$gofin_exit"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

for gofin_directory in /Library /Library/Fin "$gofin_runtime"; do
  [ -d "$gofin_directory" ] || fail 'Existing protected runtime is required; this script does not install GoFin.'
  check_protected_path "$gofin_directory"
done
for gofin_component in $gofin_old_files; do check_protected_path "$gofin_runtime/$gofin_component"; done
[ "$(/usr/bin/stat -f '%d' "$gofin_runtime")" = "$(/usr/bin/stat -f '%d' /Library/Fin)" ] || fail 'Runtime and staging must share a filesystem for atomic file replacement.'
/bin/mkdir "$gofin_update_lock" || fail 'Another runtime update or an interrupted update needs inspection; preserve existing backups.'
gofin_locked=1
check_bundle "$gofin_from" "$gofin_old_name"
check_bundle "$gofin_to" gofin
check_bundle "$gofin_runtime" "$gofin_old_name"
if [ -e "$gofin_auth_dir" ] || [ -L "$gofin_auth_dir" ]; then
  check_auth_helper "$gofin_old_name" "$gofin_old_display"
  gofin_auth_initial=1
else
  [ ! -e "$gofin_old_auth_helper" ] && [ ! -L "$gofin_old_auth_helper" ] || fail 'Installed authorization helper path is inconsistent.'
fi
for gofin_component in $gofin_old_files; do
  /usr/bin/cmp -s "$gofin_from/$gofin_component" "$gofin_runtime/$gofin_component" || fail 'Installed runtime does not exactly match --from; preserve it and review the difference.'
done
if [ "$gofin_presentation_update" = 0 ]; then
  [ "$gofin_old_name" = gofin ] || fail 'Renaming Fin requires the explicit --presentation-update flag.'
  for gofin_component in gofin-touch-id install.sh; do
    /usr/bin/cmp -s "$gofin_from/$(old_component "$gofin_component")" "$gofin_to/$gofin_component" || fail 'Helper/installer changes require the explicit --presentation-update flag.'
  done
fi
gofin_architecture=$(/usr/bin/uname -m)
for gofin_bundle in "$gofin_from" "$gofin_to"; do
  /usr/bin/awk -F= -v expected="$gofin_architecture" '$1 == "architecture" {count++; if ($2 != expected) bad=1} END {exit count != 1 || bad}' "$gofin_bundle/BUILDINFO" || fail 'Bundle architecture does not match this Mac.'
done
check_idle_clients
if [ "$gofin_old_name" = fin ]; then
  for gofin_directory in /Library/LaunchDaemons /private /private/etc /private/etc/paths.d; do
    [ -d "$gofin_directory" ] || fail 'Existing protected service and PATH registration are required for a rename.'
    check_protected_path "$gofin_directory"
  done
  for gofin_registration in "$gofin_plist" "$gofin_path_file"; do
    [ -f "$gofin_registration" ] || fail 'Existing protected service and PATH registration are required for a rename.'
    check_protected_path "$gofin_registration"
  done
  gofin_service_status=0
  /bin/launchctl print system/io.fin-cli.broker >/dev/null 2>&1 || gofin_service_status=$?
  case "$gofin_service_status" in
    0) gofin_service_initial=1 ;;
    3|113) ;;
    *) fail 'Could not inspect the existing service registration; runtime was preserved.' ;;
  esac
fi

# Staging and backup contain public runtime files and, during a rename, the
# launchd registration. No writes descend into private/ or run/.
gofin_backup=$(/usr/bin/mktemp -d /Library/Fin/runtime-backup.XXXXXX)
/bin/mkdir "$gofin_backup/previous" "$gofin_backup/incoming" "$gofin_backup/restore"
for gofin_component in $gofin_files; do
  /bin/cp -P "$gofin_runtime/$(old_component "$gofin_component")" "$gofin_backup/previous/$(old_component "$gofin_component")"
  /bin/cp -P "$gofin_to/$gofin_component" "$gofin_backup/incoming/$gofin_component"
done
/bin/cp -P "$gofin_backup/incoming/gofin-touch-id" "$gofin_backup/incoming-GoFin"
if [ "$gofin_auth_initial" = 1 ]; then
  /bin/cp -P "$gofin_old_auth_helper" "$gofin_backup/previous-GoFin"
fi
if [ "$gofin_old_name" = fin ]; then
  /bin/cp -P "$gofin_plist" "$gofin_backup/previous.plist"
  /usr/bin/python3 -I - "$gofin_backup/previous.plist" "$gofin_backup/incoming.plist" "$gofin_runtime" "$gofin_path_file" <<'PYPLIST' || fail 'Existing service or PATH registration is incompatible; runtime was preserved.'
import pathlib, plistlib, sys
try:
    source, target, runtime, registration = map(pathlib.Path, sys.argv[1:])
    if source.is_symlink() or not source.is_file(): raise ValueError()
    service = plistlib.loads(source.read_bytes())
    if service.get('Label') != 'io.fin-cli.broker': raise ValueError()
    if service.get('ProgramArguments') != [str(runtime / 'fin-broker')]: raise ValueError()
    if service.get('WorkingDirectory') != str(runtime): raise ValueError()
    socket = service['Sockets']['Broker']
    if socket.get('SockPathName') != str(runtime.parent / 'run/broker.sock'): raise ValueError()
    if type(socket.get('SockPathOwner')) is not int or not 501 <= socket['SockPathOwner'] < 2**32: raise ValueError()
    if registration.read_text() != str(runtime) + '\n': raise ValueError()
    service['ProgramArguments'] = [str(runtime / 'gofin-broker')]
    with target.open('xb') as stream: plistlib.dump(service, stream)
except Exception:
    sys.exit(1)
PYPLIST
  /bin/chmod 644 "$gofin_backup/previous.plist" "$gofin_backup/incoming.plist"
fi
# Reject any source replaced by a symlink during copying before recursive mode
# or ownership changes can encounter it. The staging parent is already root 700.
check_bundle "$gofin_backup/previous" "$gofin_old_name"
check_bundle "$gofin_backup/incoming" gofin
/usr/sbin/chown -R root:wheel "$gofin_backup"
/bin/chmod -RN "$gofin_backup"
/bin/chmod 700 "$gofin_backup" "$gofin_backup/restore"
/bin/chmod 755 "$gofin_backup/incoming-GoFin"
if [ "$gofin_auth_initial" = 1 ]; then /bin/chmod 755 "$gofin_backup/previous-GoFin"; fi
for gofin_bundle in "$gofin_backup/previous" "$gofin_backup/incoming"; do
  /bin/chmod 755 "$gofin_bundle"
  gofin_base=gofin
  if [ "$gofin_bundle" = "$gofin_backup/previous" ]; then gofin_base=$gofin_old_name; fi
  /bin/chmod 755 "$gofin_bundle/$gofin_base" "$gofin_bundle/$gofin_base-broker" "$gofin_bundle/$gofin_base-touch-id" "$gofin_bundle/install.sh"
  /bin/chmod 644 "$gofin_bundle/README.md" "$gofin_bundle/BUILDINFO" "$gofin_bundle/SHA256SUMS"
  check_bundle "$gofin_bundle" "$gofin_base"
  /usr/bin/awk -F= -v expected="$gofin_architecture" '$1 == "architecture" {count++; if ($2 != expected) bad=1} END {exit count != 1 || bad}' "$gofin_bundle/BUILDINFO" || fail 'Staged bundle architecture does not match this Mac.'
  check_code "$gofin_bundle" "$gofin_base"
done
check_code_path "$gofin_backup/incoming-GoFin"
/usr/bin/cmp -s "$gofin_backup/incoming-GoFin" "$gofin_backup/incoming/gofin-touch-id" || fail 'Staged GoFin authorization helper differs from its signed source component.'
if [ "$gofin_presentation_update" = 0 ]; then
  for gofin_component in gofin-touch-id install.sh; do
    /usr/bin/cmp -s "$gofin_backup/previous/$(old_component "$gofin_component")" "$gofin_backup/incoming/$gofin_component" || fail 'Staged helper/installer changed; pass --presentation-update only after reviewing these presentation changes.'
  done
fi
for gofin_component in $gofin_files; do
  /usr/bin/cmp -s "$gofin_from/$(old_component "$gofin_component")" "$gofin_backup/previous/$(old_component "$gofin_component")" || fail 'Old bundle/runtime changed during staging.'
  /usr/bin/cmp -s "$gofin_to/$gofin_component" "$gofin_backup/incoming/$gofin_component" || fail 'New bundle changed during staging.'
  if [ "$(old_component "$gofin_component")" != "$gofin_component" ] || ! /usr/bin/cmp -s "$gofin_backup/previous/$(old_component "$gofin_component")" "$gofin_backup/incoming/$gofin_component"; then
    gofin_changed="$gofin_changed $gofin_component"
  fi
done
if [ "$gofin_old_name" = fin ] || [ "$gofin_auth_initial" = 0 ] || ! /usr/bin/cmp -s "$gofin_backup/previous-GoFin" "$gofin_backup/incoming-GoFin"; then
  gofin_auth_changed=1
fi
[ -n "$gofin_changed" ] || [ "$gofin_auth_changed" = 1 ] || fail 'Bundles are identical; no runtime update is needed.'

# Read only the schema contract from the already verified incoming broker. The
# broker's export path is metadata-only and runs before protected state access.
gofin_storage_contract=$("$gofin_backup/incoming/gofin-broker" --export-storage-contract) || fail 'Incoming broker could not report its journal schema; runtime and journal were preserved.'
gofin_supported_schema=$(printf '%s' "$gofin_storage_contract" | /usr/bin/python3 -I -c 'import json,sys
try:
 value=json.load(sys.stdin)
 version=value["journal_schema_version"]
 if type(version) is not int or version <= 0: raise ValueError()
 print(version)
except Exception:
 sys.exit(1)') || fail 'Incoming broker reported an invalid journal schema contract; runtime and journal were preserved.'
gofin_script_dir=$(/usr/bin/dirname "$0")
/usr/bin/python3 -I "$gofin_script_dir/check-runtime-store-compatibility.py" --supported-schema "$gofin_supported_schema" >/dev/null || fail 'Journal schema is incompatible or could not be checked safely; runtime and journal were preserved.'

check_idle_clients
for gofin_component in $gofin_old_files; do
  /usr/bin/cmp -s "$gofin_backup/previous/$gofin_component" "$gofin_runtime/$gofin_component" || fail 'Installed runtime changed during staging.'
done
if [ "$gofin_old_name" = fin ]; then
  /usr/bin/cmp -s "$gofin_backup/previous.plist" "$gofin_plist" || fail 'Service registration changed during staging.'
fi
echo 'Updating GoFin...'
gofin_replacing=1
if [ "$gofin_old_name" = fin ] && [ "$gofin_service_initial" = 1 ]; then
  gofin_service_detached=1
  /bin/launchctl bootout system/io.fin-cli.broker || fail 'Could not detach the stopped service for its rename.'
fi
for gofin_component in $gofin_changed; do
  # Same-filesystem rename replaces the directory entry, never an executing file's bytes.
  /bin/mv -f "$gofin_backup/incoming/$gofin_component" "$gofin_runtime/$gofin_component"
done
if [ "$gofin_auth_changed" = 1 ]; then
  if [ "$gofin_auth_initial" = 0 ]; then
    # Set before mkdir so signal cleanup can remove a partial directory.
    gofin_auth_dir_created=1
    /bin/mkdir "$gofin_auth_dir"
    /usr/sbin/chown root:wheel "$gofin_auth_dir"
    /bin/chmod -N "$gofin_auth_dir"
    /bin/chmod 755 "$gofin_auth_dir"
  fi
  /bin/mv -f "$gofin_backup/incoming-GoFin" "$gofin_auth_helper"
fi
if [ "$gofin_old_name" = fin ]; then
  gofin_plist_changed=1
  /bin/mv -f "$gofin_backup/incoming.plist" "$gofin_plist"
  /bin/rm -f "$gofin_runtime/fin" "$gofin_runtime/fin-broker" "$gofin_runtime/fin-touch-id" "$gofin_auth_dir/Fin"
fi
check_bundle "$gofin_runtime" gofin
check_code "$gofin_runtime" gofin
if [ -e "$gofin_auth_dir" ] || [ -L "$gofin_auth_dir" ]; then check_auth_helper gofin GoFin; fi
if [ "$gofin_old_name" = fin ] && [ "$gofin_service_initial" = 1 ]; then
  /bin/launchctl bootstrap system "$gofin_plist" || fail 'Could not register the renamed service; restoring the previous runtime.'
fi
gofin_complete=1
/bin/rm -rf "$gofin_backup/incoming" "$gofin_backup/restore"
/bin/rm -f "$gofin_backup/incoming-GoFin" "$gofin_backup/restore-GoFin" "$gofin_backup/incoming.plist" "$gofin_backup/restore.plist"
if [ "${NO_COLOR+x}" != x ] && [ -t 1 ] && [ "${TERM:-dumb}" != dumb ]; then
  printf '\033[32m✓ GoFin updated.\033[0m\n'
else
  printf '✓ GoFin updated.\n'
fi
# This optional user-file maintenance cannot roll back a verified runtime update.
# Never execute user content as root or inherit the updater's environment.
# Refresh only already opted-in managed completions; unmanaged and edited
# user files are preserved by the unprivileged ownership-aware helper.
if ! /usr/bin/sudo -n -H -u "#$SUDO_UID" /usr/bin/env -i \
  "$gofin_runtime/gofin" internal-completions refresh >/dev/null; then
  echo 'GoFin updated, but optional shell completion refresh needs attention. Run gofin internal-completions refresh from your account.' >&2
fi
if ! /usr/bin/sudo -n -H -u "#$SUDO_UID" /usr/bin/env -i \
  "$gofin_runtime/gofin" internal-skills-refresh; then
  echo 'GoFin updated, but optional skill refresh needs attention. Run gofin skill inspect --agent codex.' >&2
fi
