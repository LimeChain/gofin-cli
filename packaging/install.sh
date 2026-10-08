#!/bin/bash
# Human initial setup. Never overwrite existing financial state.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH
fin_uid=''
fin_gid=''
fin_verbose=0
usage() { echo 'Usage: install.sh --approver-uid UID --approver-gid GID [--verbose]' >&2; exit 2; }
run_stage() {
  local label=$1 pid frame log status animate
  shift
  log=$(/usr/bin/mktemp /Library/.fin-setup.XXXXXX)
  if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then animate=1; else animate=0; fi
  "$@" >"$log" 2>&1 &
  pid=$!
  fin_active_pid=$pid
  fin_active_log=$log
  frame='⠋'
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$animate" = 1 ]; then
      printf '\r\033[2K\033[36m%s %s\033[0m' "$frame" "$label" >&2
      case "$frame" in '⠋') frame='⠙' ;; '⠙') frame='⠹' ;; '⠹') frame='⠸' ;; *) frame='⠋' ;; esac
    fi
    /bin/sleep 0.12
  done
  if wait "$pid"; then
    if [ "$animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[32m✓ %s\033[0m\n' "$label" >&2; else printf '✓ %s\n' "$label" >&2; fi
    if [ "$fin_verbose" = 1 ]; then /bin/cat "$log" >&2; fi
    fin_active_pid=''
    fin_active_log=''
    /bin/rm -f "$log"
  else
    status=$?
    if [ "$animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[31m✗ %s\033[0m\n' "$label" >&2; else printf '✗ %s\n' "$label" >&2; fi
    /bin/cat "$log" >&2
    fin_active_pid=''
    fin_active_log=''
    /bin/rm -f "$log"
    return "$status"
  fi
}
# Optional user-file work after protected setup has succeeded. No default opt-in,
# no spinner during input, and no privileged writes to the agent's skill files.
offer_companion_skill() {
  local fin_skill_answer=''
  [ -t 0 ] && [ -t 1 ] && [ -t 2 ] || return 0
  printf "Install Fin's optional Codex skill? [y/N] " >&2
  if ! IFS= read -r fin_skill_answer; then
    printf '\n' >&2
    return 0
  fi
  case "$fin_skill_answer" in
    y|Y|yes|YES|Yes)
      local fin_skill_env=("TERM=${TERM:-dumb}")
      if [ "${NO_COLOR+x}" = x ]; then fin_skill_env+=("NO_COLOR=$NO_COLOR"); fi
      if ! /usr/bin/sudo -n -H -u "#$fin_uid" /usr/bin/env "${fin_skill_env[@]}" \
        /Library/Fin/runtime/fin skill install --agent codex; then
        echo 'Fin is installed; optional skill installation did not finish. Run fin skill inspect --agent codex.' >&2
      fi
      ;;
  esac
}
fin_active_pid=''
fin_active_log=''
cancel_active_stage() {
  local status=$1
  if [ -n "$fin_active_pid" ]; then
    kill -TERM "$fin_active_pid" 2>/dev/null || true
    wait "$fin_active_pid" 2>/dev/null || true
  fi
  if [ -n "$fin_active_log" ]; then /bin/rm -f "$fin_active_log"; fi
  exit "$status"
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --verbose) fin_verbose=1; shift ;;
    --approver-uid|--approver-gid)
      [ "$#" -ge 2 ] || usage
      case "$1" in
        --approver-uid) fin_uid=$2 ;;
        --approver-gid) fin_gid=$2 ;;
      esac
      shift 2
      ;;
    *) usage ;;
  esac
done
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'Protected installation requires macOS.' >&2; exit 1; }
[ "$(/usr/bin/id -u)" = 0 ] || { echo 'Run fin setup yourself to request administrator authorization.' >&2; exit 1; }
case "$fin_uid:$fin_gid" in *[!0-9:]*|:|:*|*:) usage ;; esac
[ "$fin_uid" -ge 501 ] && [ "$fin_gid" -gt 0 ] || usage
/usr/bin/id -nu "$fin_uid" >/dev/null
[ "$(/usr/bin/id -g "$fin_uid")" = "$fin_gid" ] || usage
[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Setup requires a fresh human Terminal without pipelines or redirection.' >&2; exit 1; }
fin_source=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
fin_root=/Library/Fin
fin_plist=/Library/LaunchDaemons/io.fin-cli.broker.plist
fin_path_file=/private/etc/paths.d/fin
for fin_existing in "$fin_root" "$fin_plist" /usr/local/bin/fin "$fin_path_file"; do
  [ ! -e "$fin_existing" ] && [ ! -L "$fin_existing" ] || { echo 'An existing Fin installation needs a reviewed upgrade; its state was preserved.' >&2; exit 1; }
done
# /etc is a macOS symlink. Check its real path without modifying system dirs.
check_path_directories() {
  for fin_directory in /private /private/etc /private/etc/paths.d; do
    [ -d "$fin_directory" ] && [ ! -L "$fin_directory" ] && [ "$(/usr/bin/stat -f '%u' "$fin_directory")" = 0 ] || { echo 'PATH registration requires existing real root-owned system directories; installation state was preserved.' >&2; return 1; }
    /bin/ls -ld "$fin_directory" | /usr/bin/awk '{if (substr($1,6,1)=="w" || substr($1,9,1)=="w" || index($1,"+")) bad=1} END {exit bad}' || { echo 'PATH registration requires system directories without group/world write access or extended ACLs; installation state was preserved.' >&2; return 1; }
  done
}
check_path_directories
for fin_name in fin fin-broker fin-touch-id install.sh README.md BUILDINFO SHA256SUMS; do
  [ -f "$fin_source/$fin_name" ] && [ ! -L "$fin_source/$fin_name" ] || { echo 'Bundle has a missing or symbolic-link component.' >&2; exit 1; }
done
/usr/bin/awk 'NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ {bad=1}
 $2 != "fin" && $2 != "fin-broker" && $2 != "fin-touch-id" && $2 != "install.sh" && $2 != "README.md" && $2 != "BUILDINFO" {bad=1}
 {if (++seen[$2] > 1) bad=1; count++} END {if (count != 6) bad=1; exit bad}' "$fin_source/SHA256SUMS" || { echo 'Bundle checksum manifest is invalid.' >&2; exit 1; }
if [ "$fin_verbose" = 1 ]; then
  echo "Fin setup: bundle $fin_source" >&2
  echo "Fin setup: runtime $fin_root/runtime" >&2
  echo "Fin setup: PATH registration $fin_path_file" >&2
fi
fin_stage=$(/usr/bin/mktemp -d /Library/.fin-rust-install.XXXXXX)
fin_installed=0
fin_configured=0
cleanup() {
  /bin/rm -rf "$fin_stage"
  # Initialization can commit configuration and then report a sync/store failure.
  # Preserve that uncertain durable result; never erase it as cancellation.
  if [ "$fin_installed" = 1 ] && [ "$fin_configured" = 0 ]; then
    if [ -e "$fin_root/private/config.json" ] || [ -L "$fin_root/private/config.json" ]; then
      echo 'Configuration may already be committed. Protected installation was preserved; inspect it before retrying setup.' >&2
    else
      /bin/rm -rf "$fin_root"
    fi
  fi
}
trap cleanup EXIT
trap 'cancel_active_stage 129' HUP
trap 'cancel_active_stage 130' INT
trap 'cancel_active_stage 143' TERM
prepare_runtime() {
  /bin/mkdir "$fin_stage/runtime" "$fin_stage/private" "$fin_stage/run"
  /bin/mkdir "$fin_stage/runtime/authorization"
  /bin/mkdir "$fin_stage/private/credentials"
  # Enrollment requires this fixed private lock before any configuration exists.
  : > "$fin_stage/private/maintenance.lock"
  for fin_name in fin fin-broker fin-touch-id install.sh README.md BUILDINFO SHA256SUMS; do /bin/cp "$fin_source/$fin_name" "$fin_stage/runtime/$fin_name"; done
  /bin/cp "$fin_stage/runtime/fin-touch-id" "$fin_stage/runtime/authorization/Fin"
  /usr/bin/cmp -s "$fin_stage/runtime/fin-touch-id" "$fin_stage/runtime/authorization/Fin" || { echo 'Human-named authorization helper copy does not match the signed bundle helper.' >&2; return 1; }
  (cd "$fin_stage/runtime" && /usr/bin/shasum -a 256 -c SHA256SUMS >/dev/null) || { echo 'Copied checksum verification failed.' >&2; return 1; }
  /usr/sbin/chown -R root:wheel "$fin_stage"
  /bin/chmod -RN "$fin_stage"
  /bin/chmod 755 "$fin_stage" "$fin_stage/runtime" "$fin_stage/runtime/authorization" "$fin_stage/run"
  /bin/chmod 700 "$fin_stage/private" "$fin_stage/private/credentials"
  /bin/chmod 600 "$fin_stage/private/maintenance.lock"
  /bin/chmod 644 "$fin_stage/runtime/README.md" "$fin_stage/runtime/BUILDINFO" "$fin_stage/runtime/SHA256SUMS"
  /bin/chmod 755 "$fin_stage/runtime/fin" "$fin_stage/runtime/fin-broker" "$fin_stage/runtime/fin-touch-id" "$fin_stage/runtime/authorization/Fin" "$fin_stage/runtime/install.sh"
}
verify_binaries() {
  for fin_binary in "$fin_stage/runtime/fin" "$fin_stage/runtime/fin-broker" "$fin_stage/runtime/fin-touch-id" "$fin_stage/runtime/authorization/Fin"; do
    /usr/bin/codesign --verify --strict "$fin_binary"
    /usr/bin/codesign -d --verbose=4 "$fin_binary" 2>&1 | /usr/bin/awk '/^CodeDirectory / && /flags=0x[0-9a-f]+\([^)]*runtime[,)]/ {valid=1} END {exit !valid}' || { echo 'Bundle lacks hardened signing.' >&2; return 1; }
    /usr/bin/otool -L "$fin_binary" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}' || { echo 'Bundle uses unprotected non-OS libraries.' >&2; return 1; }
  done
}
run_stage 'Preparing protected runtime files' prepare_runtime
run_stage 'Verifying signed binaries and system libraries' verify_binaries
/bin/mv "$fin_stage" "$fin_root"
fin_installed=1
if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[32m✓ Installing protected runtime\033[0m\n' >&2; else printf '✓ Installing protected runtime\n' >&2; fi
# Enrollment verifies all three descriptors. Keep stdout attached to the human
# terminal as well; suppressing it would make the protected input check fail.
if [ "$fin_verbose" = 1 ]; then
  /Library/Fin/runtime/fin-broker enrollment initialize --request "{\"approver_uid\":$fin_uid}" --verbose
else
  /Library/Fin/runtime/fin-broker enrollment initialize --request "{\"approver_uid\":$fin_uid}"
fi
# Preserve approved configuration and journal even if later service setup fails.
fin_configured=1
install_service() {
/bin/cat > "$fin_plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>io.fin-cli.broker</string>
<key>ProgramArguments</key><array><string>/Library/Fin/runtime/fin-broker</string></array>
<key>WorkingDirectory</key><string>/Library/Fin/runtime</string>
<key>UserName</key><string>root</string><key>GroupName</key><string>wheel</string>
<key>Umask</key><integer>63</integer>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>/usr/bin:/bin</string><key>LC_ALL</key><string>C</string></dict>
<key>Sockets</key><dict><key>Broker</key><dict>
<key>SockPathName</key><string>/Library/Fin/run/broker.sock</string>
<key>SockPathOwner</key><integer>$fin_uid</integer><key>SockPathGroup</key><integer>$fin_gid</integer><key>SockPathMode</key><integer>384</integer>
<key>SockType</key><string>stream</string>
</dict></dict>
<key>StandardOutPath</key><string>/Library/Fin/private/broker.stdout.log</string>
<key>StandardErrorPath</key><string>/Library/Fin/private/broker.stderr.log</string>
</dict></plist>
PLIST
/usr/sbin/chown root:wheel "$fin_plist"
/bin/chmod -N "$fin_plist"
/bin/chmod 644 "$fin_plist"
/usr/bin/plutil -lint "$fin_plist" >/dev/null
/bin/launchctl bootstrap system "$fin_plist"
}
run_stage 'Registering protected local broker service' install_service
start_broker() {
  # Use the configured human's authenticated client, not a root socket client.
  # daemon start checks the broker reply and observed launchd process state.
  /usr/bin/sudo -n -H -u "#$fin_uid" /usr/bin/env TERM=dumb \
    /Library/Fin/runtime/fin daemon start
}
run_stage 'Starting and checking Fin daemon' start_broker
# Register only after approved configuration and service bootstrap. Failures
# retain the protected configuration/journal for inspection rather than retry.
register_fin_command() {
check_path_directories
[ ! -e "$fin_path_file" ] && [ ! -L "$fin_path_file" ] || { echo 'The Fin PATH registration already exists. Protected installation was preserved; inspect it before retrying setup.' >&2; exit 1; }
(set -o noclobber; printf '%s\n' /Library/Fin/runtime > "$fin_path_file") || { echo 'Fin PATH registration failed. Protected installation was preserved; inspect it before retrying setup.' >&2; exit 1; }
/usr/sbin/chown root:wheel "$fin_path_file"
/bin/chmod -N "$fin_path_file"
/bin/chmod 644 "$fin_path_file"
}
run_stage 'Registering Fin for new Terminal sessions' register_fin_command
# The completion helper runs as the configured interactive account so its
# getpwuid-based home and shell selection cannot be redirected by root env.
fin_completion_marker=/Library/Fin/private/completion-owner
if (umask 077; printf '%s\n' "$fin_uid" > "$fin_completion_marker") &&
   /usr/sbin/chown root:wheel "$fin_completion_marker" &&
   /bin/chmod -N "$fin_completion_marker" &&
   /bin/chmod 600 "$fin_completion_marker"; then
  fin_completion_env=("TERM=${TERM:-dumb}")
  if [ "${NO_COLOR+x}" = x ]; then fin_completion_env+=("NO_COLOR=$NO_COLOR"); fi
  if ! /usr/bin/sudo -n -H -u "#$fin_uid" /usr/bin/env "${fin_completion_env[@]}" \
    /Library/Fin/runtime/fin internal-completions install; then
    echo 'Fin is installed, but shell completion setup did not finish. Run `fin internal-completions install` from your account.' >&2
  fi
else
  /bin/rm -f "$fin_completion_marker" || true
  echo 'Fin is installed, but shell completion setup was skipped because its owner marker could not be secured.' >&2
fi
echo 'Fin installed and running. Next: open a new Terminal and run fin connections add.'
offer_companion_skill
if [ "$fin_verbose" = 1 ]; then
  echo 'Fin setup: installed runtime: /Library/Fin/runtime' >&2
  echo 'Fin setup: protected configuration and operation journal: /Library/Fin/private' >&2
  echo 'Fin setup: broker socket: /Library/Fin/run/broker.sock' >&2
  echo 'Fin setup: run sudo -K and close elevated shells after setup.' >&2
  echo 'Fin setup: remove agent-readable credential copies after protected enrollment; rotate any keys that were disclosed.' >&2
  echo 'Fin setup: run fin doctor --json and fin connections add.' >&2
fi
