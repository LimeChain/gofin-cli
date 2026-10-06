#!/bin/bash
# Reviewed local-beta runtime update only: same API/config/store/financial
# behavior. Helper/installer changes require an explicit human-selected
# --presentation-update flag and still pass the same bundle checks.
# Human: stop issuing Fin commands and finish/cancel enrollment first. Review
# this script and both bundles, then run in your own fresh macOS Terminal:
#   sudo -k /bin/bash /absolute/path/to/this-script --from OLD_BUNDLE --to NEW_BUNDLE
#   sudo -K
# No Fin command, service restart, private-state access, or financial call occurs.
# Existing daemon processes keep their loaded image; enrollment starts the new
# broker directly. Process checks are advisory, not a synchronization guarantee.
# Per-file renames are atomic, not the entire bundle; ordinary partial failures
# restore changed files. A power loss/SIGKILL needs inspection using the retained
# runtime-only backup. Never rerun setup or erase private state to recover.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH

fail() { echo "$*" >&2; exit 1; }
usage() { echo 'Usage: sudo -k /bin/bash update-rust-macos-runtime.sh --from /reviewed/old-bundle --to /reviewed/new-bundle [--presentation-update]' >&2; exit 2; }
fin_from=''
fin_to=''
fin_presentation_update=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --presentation-update) [ "$fin_presentation_update" = 0 ] || usage; fin_presentation_update=1; shift; continue ;;
    --from|--to)
      [ "$#" -ge 2 ] || usage
      case "$1" in
        --from) [ -z "$fin_from" ] || usage; fin_from=$2 ;;
        --to) [ -z "$fin_to" ] || usage; fin_to=$2 ;;
      esac
      shift 2
      ;;
    *) usage ;;
  esac
done
case "$fin_from" in /*) ;; *) usage ;; esac
case "$fin_to" in /*) ;; *) usage ;; esac
[ "$(/usr/bin/uname -s)" = Darwin ] || fail 'This runtime hotfix requires macOS.'
[ "$(/usr/bin/id -u)" = 0 ] || fail 'The human must run this reviewed script with fresh sudo -k in their own Terminal.'
case "${SUDO_UID:-}" in ''|*[!0-9]*) fail 'Run through human sudo, not an elevated shell.' ;; esac
[ "$SUDO_UID" -ge 501 ] || fail 'Run through human sudo, not an elevated shell.'
[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || fail 'Use a fresh human Terminal without pipelines or redirection.'

fin_runtime=/Library/Fin/runtime
fin_auth_dir=$fin_runtime/authorization
fin_auth_helper=$fin_auth_dir/Fin
fin_files='fin fin-broker fin-touch-id install.sh README.md BUILDINFO SHA256SUMS'
fin_changed=''
fin_backup=''
fin_replacing=0
fin_complete=0
fin_locked=0
fin_auth_initial=0
fin_auth_changed=0
fin_auth_dir_created=0
fin_update_lock=/Library/Fin/.runtime-update.lock

check_protected_path() {
  [ ! -L "$1" ] && [ "$(/usr/bin/stat -f '%u' "$1")" = 0 ] || fail 'Installed runtime paths must be real and root-owned.'
  /bin/ls -ld "$1" | /usr/bin/awk '{if (substr($1,6,1)=="w" || substr($1,9,1)=="w" || index($1,"+")) bad=1} END {exit bad}' || fail 'Installed runtime paths must have no group/world write access or extended ACLs.'
}

check_bundle() {
  [ -d "$1" ] && [ ! -L "$1" ] || fail 'Bundle/runtime must be a real directory.'
  for fin_component in $fin_files; do
    [ -f "$1/$fin_component" ] && [ ! -L "$1/$fin_component" ] || fail 'Bundle/runtime contains a missing, nonregular, or symbolic-link component.'
  done
  for fin_entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
    [ -e "$fin_entry" ] || [ -L "$fin_entry" ] || continue
    case "${fin_entry##*/}" in
      fin|fin-broker|fin-touch-id|install.sh|README.md|BUILDINFO|SHA256SUMS) ;;
      authorization) [ "$1" = "$fin_runtime" ] && [ -d "$fin_entry" ] && [ ! -L "$fin_entry" ] || fail 'This hotfix only supports the fixed seven-component beta bundle and its derived authorization helper.' ;;
      *) fail 'This hotfix only supports the fixed seven-component beta bundle.' ;;
    esac
  done
  /usr/bin/awk 'NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ {bad=1}
    $2 != "fin" && $2 != "fin-broker" && $2 != "fin-touch-id" && $2 != "install.sh" && $2 != "README.md" && $2 != "BUILDINFO" {bad=1}
    {if (++seen[$2] > 1) bad=1; count++} END {if (count != 6) bad=1; exit bad}' "$1/SHA256SUMS" || fail 'Bundle checksum manifest is invalid.'
  (cd "$1" && /usr/bin/shasum -a 256 -c SHA256SUMS >/dev/null) || fail 'Bundle/runtime checksum verification failed.'
}

check_code_path() {
  /usr/bin/codesign --verify --strict "$1" || fail 'Bundle code signature verification failed.'
  /usr/bin/codesign -d --verbose=4 "$1" 2>&1 | /usr/bin/awk '/^CodeDirectory / && /flags=0x[0-9a-f]+\([^)]*runtime[,)]/ {valid=1} END {exit !valid}' || fail 'Bundle lacks hardened signing.'
  /usr/bin/otool -L "$1" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}' || fail 'Bundle uses non-OS libraries.'
}

check_code() {
  for fin_binary in fin fin-broker fin-touch-id; do
    check_code_path "$1/$fin_binary"
  done
}

check_auth_helper() {
  [ -d "$fin_auth_dir" ] && [ ! -L "$fin_auth_dir" ] || fail 'Installed authorization helper directory is not a protected real directory.'
  [ -f "$fin_auth_helper" ] && [ ! -L "$fin_auth_helper" ] || fail 'Installed Fin authorization helper is missing or not a regular file.'
  check_protected_path "$fin_auth_dir"
  check_protected_path "$fin_auth_helper"
  [ "$(/usr/bin/stat -f '%g' "$fin_auth_dir")" = 0 ] || fail 'Installed authorization helper directory group is invalid.'
  [ "$(/usr/bin/stat -f '%g' "$fin_auth_helper")" = 0 ] || fail 'Installed Fin authorization helper group is invalid.'
  [ "$(/usr/bin/stat -f '%Lp' "$fin_auth_dir")" = 755 ] || fail 'Installed authorization helper directory permissions are invalid.'
  [ "$(/usr/bin/stat -f '%Lp' "$fin_auth_helper")" = 755 ] || fail 'Installed Fin authorization helper permissions are invalid.'
  for fin_entry in "$fin_auth_dir"/* "$fin_auth_dir"/.[!.]* "$fin_auth_dir"/..?*; do
    [ -e "$fin_entry" ] || [ -L "$fin_entry" ] || continue
    [ "$fin_entry" = "$fin_auth_helper" ] || fail 'Installed authorization helper directory contains an unexpected component.'
  done
  /usr/bin/cmp -s "$fin_auth_helper" "$fin_runtime/fin-touch-id" || fail 'Installed Fin authorization helper differs from its signed source component.'
  check_code_path "$fin_auth_helper"
}

check_idle_clients() {
  # pgrep emits only PIDs (discarded), never other processes' arguments.
  local fin_status=0
  /usr/bin/pgrep -x 'fin|fin-touch-id|Fin' >/dev/null || fin_status=$?
  [ "$fin_status" = 1 ] || fail 'A Fin client/approval is active or process inspection failed; finish it and retry.'
  fin_status=0
  /usr/bin/pgrep -f '(^|/)fin-broker[[:space:]]+enrollment([[:space:]]|$)' >/dev/null || fin_status=$?
  [ "$fin_status" = 1 ] || fail 'Enrollment is active or process inspection failed; finish/cancel it and retry.'
}

cleanup() {
  local fin_exit=$?
  trap - EXIT HUP INT TERM
  if [ "$fin_replacing" = 1 ] && [ "$fin_complete" = 0 ]; then
    echo 'Update failed/interrupted; restoring changed runtime files from the verified backup.' >&2
    local fin_restore_ok=1
    for fin_component in $fin_changed; do
      if ! /bin/cp -p "$fin_backup/previous/$fin_component" "$fin_backup/restore/$fin_component" ||
         ! /bin/mv -f "$fin_backup/restore/$fin_component" "$fin_runtime/$fin_component"; then
        fin_restore_ok=0
      fi
    done
    if [ "$fin_auth_changed" = 1 ]; then
      if [ "$fin_auth_initial" = 1 ]; then
        if ! /bin/cp -p "$fin_backup/previous-Fin" "$fin_backup/restore-Fin" ||
           ! /bin/mv -f "$fin_backup/restore-Fin" "$fin_auth_helper"; then
          fin_restore_ok=0
        fi
      else
        /bin/rm -f "$fin_auth_helper" || fin_restore_ok=0
      fi
      if [ "$fin_auth_dir_created" = 1 ] && [ -d "$fin_auth_dir" ]; then
        /bin/rmdir "$fin_auth_dir" 2>/dev/null || fin_restore_ok=0
      fi
    fi
    for fin_component in $fin_files; do
      /usr/bin/cmp -s "$fin_backup/previous/$fin_component" "$fin_runtime/$fin_component" || fin_restore_ok=0
    done
    if [ "$fin_auth_initial" = 1 ]; then
      /usr/bin/cmp -s "$fin_backup/previous-Fin" "$fin_auth_helper" || fin_restore_ok=0
    else
      [ ! -e "$fin_auth_dir" ] && [ ! -L "$fin_auth_dir" ] || fin_restore_ok=0
    fi
    if [ "$fin_restore_ok" = 1 ]; then
      echo 'Previous runtime restored. No service was stopped.' >&2
    else
      echo 'Runtime restoration is uncertain. Stop issuing Fin commands and inspect the retained runtime backup; do not rerun setup.' >&2
    fi
    fin_exit=1
  fi
  if [ -n "$fin_backup" ]; then
    if [ "$fin_replacing" = 0 ]; then
      /bin/rm -rf "$fin_backup"
    else
      echo "Runtime-only backup retained: $fin_backup" >&2
    fi
  fi
  if [ "$fin_locked" = 1 ]; then /bin/rmdir "$fin_update_lock"; fi
  exit "$fin_exit"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

for fin_directory in /Library /Library/Fin "$fin_runtime"; do
  [ -d "$fin_directory" ] || fail 'Existing protected runtime is required; this script does not install Fin.'
  check_protected_path "$fin_directory"
done
for fin_component in $fin_files; do check_protected_path "$fin_runtime/$fin_component"; done
[ "$(/usr/bin/stat -f '%d' "$fin_runtime")" = "$(/usr/bin/stat -f '%d' /Library/Fin)" ] || fail 'Runtime and staging must share a filesystem for atomic file replacement.'
/bin/mkdir "$fin_update_lock" || fail 'Another runtime update or an interrupted update needs inspection; preserve existing backups.'
fin_locked=1
check_bundle "$fin_from"
check_bundle "$fin_to"
check_bundle "$fin_runtime"
if [ -e "$fin_auth_dir" ] || [ -L "$fin_auth_dir" ]; then
  check_auth_helper
  fin_auth_initial=1
else
  [ ! -e "$fin_auth_helper" ] && [ ! -L "$fin_auth_helper" ] || fail 'Installed authorization helper path is inconsistent.'
fi
for fin_component in $fin_files; do
  /usr/bin/cmp -s "$fin_from/$fin_component" "$fin_runtime/$fin_component" || fail 'Installed runtime does not exactly match --from; preserve it and review the difference.'
done
if [ "$fin_presentation_update" = 0 ]; then
  for fin_component in fin-touch-id install.sh; do
    /usr/bin/cmp -s "$fin_from/$fin_component" "$fin_to/$fin_component" || fail 'Helper/installer changes require the explicit --presentation-update flag.'
  done
fi
fin_architecture=$(/usr/bin/uname -m)
for fin_bundle in "$fin_from" "$fin_to"; do
  /usr/bin/awk -F= -v expected="$fin_architecture" '$1 == "architecture" {count++; if ($2 != expected) bad=1} END {exit count != 1 || bad}' "$fin_bundle/BUILDINFO" || fail 'Bundle architecture does not match this Mac.'
done
check_idle_clients

# Staging and backup contain public runtime files only. Neither reads nor writes
# descend into private/, run/, launchd configuration, or PATH registration.
fin_backup=$(/usr/bin/mktemp -d /Library/Fin/runtime-backup.XXXXXX)
/bin/mkdir "$fin_backup/previous" "$fin_backup/incoming" "$fin_backup/restore"
for fin_component in $fin_files; do
  /bin/cp -P "$fin_runtime/$fin_component" "$fin_backup/previous/$fin_component"
  /bin/cp -P "$fin_to/$fin_component" "$fin_backup/incoming/$fin_component"
done
/bin/cp -P "$fin_backup/incoming/fin-touch-id" "$fin_backup/incoming-Fin"
if [ "$fin_auth_initial" = 1 ]; then
  /bin/cp -P "$fin_auth_helper" "$fin_backup/previous-Fin"
fi
# Reject any source replaced by a symlink during copying before recursive mode
# or ownership changes can encounter it. The staging parent is already root 700.
check_bundle "$fin_backup/previous"
check_bundle "$fin_backup/incoming"
/usr/sbin/chown -R root:wheel "$fin_backup"
/bin/chmod -RN "$fin_backup"
/bin/chmod 700 "$fin_backup" "$fin_backup/restore"
/bin/chmod 755 "$fin_backup/incoming-Fin"
if [ "$fin_auth_initial" = 1 ]; then /bin/chmod 755 "$fin_backup/previous-Fin"; fi
for fin_bundle in "$fin_backup/previous" "$fin_backup/incoming"; do
  /bin/chmod 755 "$fin_bundle"
  /bin/chmod 755 "$fin_bundle/fin" "$fin_bundle/fin-broker" "$fin_bundle/fin-touch-id" "$fin_bundle/install.sh"
  /bin/chmod 644 "$fin_bundle/README.md" "$fin_bundle/BUILDINFO" "$fin_bundle/SHA256SUMS"
  check_bundle "$fin_bundle"
  /usr/bin/awk -F= -v expected="$fin_architecture" '$1 == "architecture" {count++; if ($2 != expected) bad=1} END {exit count != 1 || bad}' "$fin_bundle/BUILDINFO" || fail 'Staged bundle architecture does not match this Mac.'
  check_code "$fin_bundle"
done
check_code_path "$fin_backup/incoming-Fin"
/usr/bin/cmp -s "$fin_backup/incoming-Fin" "$fin_backup/incoming/fin-touch-id" || fail 'Staged Fin authorization helper differs from its signed source component.'
if [ "$fin_presentation_update" = 0 ]; then
  for fin_component in fin-touch-id install.sh; do
    /usr/bin/cmp -s "$fin_backup/previous/$fin_component" "$fin_backup/incoming/$fin_component" || fail 'Staged helper/installer changed; pass --presentation-update only after reviewing these presentation changes.'
  done
fi
for fin_component in $fin_files; do
  /usr/bin/cmp -s "$fin_from/$fin_component" "$fin_backup/previous/$fin_component" || fail 'Old bundle/runtime changed during staging.'
  /usr/bin/cmp -s "$fin_to/$fin_component" "$fin_backup/incoming/$fin_component" || fail 'New bundle changed during staging.'
  if ! /usr/bin/cmp -s "$fin_backup/previous/$fin_component" "$fin_backup/incoming/$fin_component"; then
    fin_changed="$fin_changed $fin_component"
  fi
done
if [ "$fin_auth_initial" = 0 ] || ! /usr/bin/cmp -s "$fin_backup/previous-Fin" "$fin_backup/incoming-Fin"; then
  fin_auth_changed=1
fi
[ -n "$fin_changed" ] || [ "$fin_auth_changed" = 1 ] || fail 'Bundles are identical; no runtime update is needed.'
check_idle_clients
for fin_component in $fin_files; do
  /usr/bin/cmp -s "$fin_backup/previous/$fin_component" "$fin_runtime/$fin_component" || fail 'Installed runtime changed during staging.'
done
echo 'Updating Fin...'
fin_replacing=1
for fin_component in $fin_changed; do
  # Same-filesystem rename replaces the directory entry, never an executing file's bytes.
  /bin/mv -f "$fin_backup/incoming/$fin_component" "$fin_runtime/$fin_component"
done
if [ "$fin_auth_changed" = 1 ]; then
  if [ "$fin_auth_initial" = 0 ]; then
    # Set before mkdir so signal cleanup can remove a partial directory.
    fin_auth_dir_created=1
    /bin/mkdir "$fin_auth_dir"
    /usr/sbin/chown root:wheel "$fin_auth_dir"
    /bin/chmod -N "$fin_auth_dir"
    /bin/chmod 755 "$fin_auth_dir"
  fi
  /bin/mv -f "$fin_backup/incoming-Fin" "$fin_auth_helper"
fi
check_bundle "$fin_runtime"
check_code "$fin_runtime"
if [ -e "$fin_auth_dir" ] || [ -L "$fin_auth_dir" ]; then check_auth_helper; fi
fin_complete=1
/bin/rm -rf "$fin_backup/incoming" "$fin_backup/restore"
/bin/rm -f "$fin_backup/incoming-Fin" "$fin_backup/restore-Fin"
echo 'Fin updated. Run sudo -K, then retry enrollment.'
