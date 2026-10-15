#!/bin/bash
# Human initial setup. Never overwrite existing financial state.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH
gofin_uid=''
gofin_gid=''
gofin_verbose=0
usage() { echo 'Usage: install.sh --approver-uid UID --approver-gid GID [--verbose]' >&2; exit 2; }
run_stage() {
  local label=$1 pid frame log status animate
  shift
  log=$(/usr/bin/mktemp /Library/.gofin-setup.XXXXXX)
  if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then animate=1; else animate=0; fi
  "$@" >"$log" 2>&1 &
  pid=$!
  gofin_active_pid=$pid
  gofin_active_log=$log
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
    if [ "$gofin_verbose" = 1 ]; then /bin/cat "$log" >&2; fi
    gofin_active_pid=''
    gofin_active_log=''
    /bin/rm -f "$log"
  else
    status=$?
    if [ "$animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[31m✗ %s\033[0m\n' "$label" >&2; else printf '✗ %s\n' "$label" >&2; fi
    /bin/cat "$log" >&2
    gofin_active_pid=''
    gofin_active_log=''
    /bin/rm -f "$log"
    return "$status"
  fi
}
# Single-key reads temporarily alter terminal settings. Restore the saved state
# both before dispatching skill installers and from the installer's EXIT trap.
restore_companion_terminal() {
  if [ -n "${gofin_skill_terminal_state:-}" ]; then
    /bin/stty "$gofin_skill_terminal_state" || return 1
    gofin_skill_terminal_state=''
  fi
}
# Optional user-file work after protected setup has succeeded. No default opt-in,
# no spinner during input, and no privileged writes to the agent's skill files.
offer_companion_skill() {
  [ -t 0 ] && [ -t 1 ] && [ -t 2 ] || return 0
  gofin_skill_terminal_state=$(/bin/stty -g) || return 0
  local gofin_skill_key='' gofin_skill_escape='' gofin_skill_focus=0
  local gofin_skill_marks=(' ' ' ') gofin_skill_cursors=('>' ' ')
  local gofin_skill_agents=()
  printf "Install GoFin's optional skill\n" >&2
  printf 'Arrows move; Space selects; Enter installs; Esc skips. None selected skips.\n' >&2
  while :; do
    gofin_skill_cursors=(' ' ' ')
    gofin_skill_cursors[$gofin_skill_focus]='>'
    # Fixed-width, input-driven redraw needs no ANSI or color. Dumb terminals
    # receive a fresh line instead of cursor movement.
    if [ "${TERM:-dumb}" != dumb ]; then printf '\r' >&2; fi
    printf '%s [%s] Codex   %s [%s] Claude Code' \
      "${gofin_skill_cursors[0]}" "${gofin_skill_marks[0]}" \
      "${gofin_skill_cursors[1]}" "${gofin_skill_marks[1]}" >&2
    if [ "${TERM:-dumb}" = dumb ]; then printf '\n' >&2; fi
    gofin_skill_key=''
    if ! IFS= read -r -s -n 1 gofin_skill_key; then
      printf '\n' >&2
      restore_companion_terminal || return 1
      return 0
    fi
    case "$gofin_skill_key" in
      '') break ;;
      $'\004') printf '\n' >&2; restore_companion_terminal || return 1; return 0 ;;
      ' ')
        if [ "${gofin_skill_marks[$gofin_skill_focus]}" = ' ' ]; then
          gofin_skill_marks[$gofin_skill_focus]='x'
        else
          gofin_skill_marks[$gofin_skill_focus]=' '
        fi
        ;;
      $'\033')
        gofin_skill_escape=''
        if ! IFS= read -r -s -n 1 -t 1 gofin_skill_escape; then
          printf '\n' >&2
          restore_companion_terminal || return 1
          return 0
        fi
        case "$gofin_skill_escape" in
          '['|'O')
            gofin_skill_escape=''
            if IFS= read -r -s -n 1 -t 1 gofin_skill_escape; then
              case "$gofin_skill_escape" in
                A|B|C|D) gofin_skill_focus=$((1 - gofin_skill_focus)) ;;
              esac
            fi
            ;;
          *) printf '\n' >&2; restore_companion_terminal || return 1; return 0 ;;
        esac
        ;;
    esac
  done
  printf '\n' >&2
  restore_companion_terminal || return 1
  if [ "${gofin_skill_marks[0]}" = x ]; then gofin_skill_agents+=(codex); fi
  if [ "${gofin_skill_marks[1]}" = x ]; then gofin_skill_agents+=(claude); fi
  # Bash 3.2 treats an empty array as unset under nounset.
  [ "${#gofin_skill_agents[@]}" -gt 0 ] || return 0
  local gofin_skill_env=("TERM=${TERM:-dumb}")
  if [ "${NO_COLOR+x}" = x ]; then gofin_skill_env+=("NO_COLOR=$NO_COLOR"); fi
  local gofin_skill_agent
  for gofin_skill_agent in "${gofin_skill_agents[@]}"; do
    if ! /usr/bin/sudo -n -H -u "#$gofin_uid" /usr/bin/env "${gofin_skill_env[@]}" \
      /Library/Fin/runtime/gofin skill install --agent "$gofin_skill_agent"; then
      echo "GoFin is installed; optional skill installation did not finish. Run gofin skill inspect --agent $gofin_skill_agent." >&2
    fi
  done
}
gofin_active_pid=''
gofin_active_log=''
cancel_active_stage() {
  local status=$1
  if [ -n "$gofin_active_pid" ]; then
    kill -TERM "$gofin_active_pid" 2>/dev/null || true
    wait "$gofin_active_pid" 2>/dev/null || true
  fi
  if [ -n "$gofin_active_log" ]; then /bin/rm -f "$gofin_active_log"; fi
  exit "$status"
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --verbose) gofin_verbose=1; shift ;;
    --approver-uid|--approver-gid)
      [ "$#" -ge 2 ] || usage
      case "$1" in
        --approver-uid) gofin_uid=$2 ;;
        --approver-gid) gofin_gid=$2 ;;
      esac
      shift 2
      ;;
    *) usage ;;
  esac
done
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'Protected installation requires macOS.' >&2; exit 1; }
[ "$(/usr/bin/id -u)" = 0 ] || { echo 'Run gofin setup yourself to request administrator authorization.' >&2; exit 1; }
case "$gofin_uid:$gofin_gid" in *[!0-9:]*|:|:*|*:) usage ;; esac
[ "$gofin_uid" -ge 501 ] && [ "$gofin_gid" -gt 0 ] || usage
/usr/bin/id -nu "$gofin_uid" >/dev/null
[ "$(/usr/bin/id -g "$gofin_uid")" = "$gofin_gid" ] || usage
[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Setup requires a fresh human Terminal without pipelines or redirection.' >&2; exit 1; }
gofin_source=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
gofin_root=/Library/Fin
gofin_plist=/Library/LaunchDaemons/io.fin-cli.broker.plist
gofin_path_file=/private/etc/paths.d/fin
for gofin_existing in "$gofin_root" "$gofin_plist" /usr/local/bin/gofin /usr/local/bin/fin "$gofin_path_file"; do
  [ ! -e "$gofin_existing" ] && [ ! -L "$gofin_existing" ] || { echo 'An existing GoFin installation needs a reviewed upgrade; its state was preserved.' >&2; exit 1; }
done
# /etc is a macOS symlink. Check its real path without modifying system dirs.
check_path_directories() {
  for gofin_directory in /private /private/etc /private/etc/paths.d; do
    [ -d "$gofin_directory" ] && [ ! -L "$gofin_directory" ] && [ "$(/usr/bin/stat -f '%u' "$gofin_directory")" = 0 ] || { echo 'PATH registration requires existing real root-owned system directories; installation state was preserved.' >&2; return 1; }
    /bin/ls -ld "$gofin_directory" | /usr/bin/awk '{if (substr($1,6,1)=="w" || substr($1,9,1)=="w" || index($1,"+")) bad=1} END {exit bad}' || { echo 'PATH registration requires system directories without group/world write access or extended ACLs; installation state was preserved.' >&2; return 1; }
  done
}
check_path_directories
for gofin_name in gofin gofin-broker gofin-touch-id install.sh README.md BUILDINFO SHA256SUMS; do
  [ -f "$gofin_source/$gofin_name" ] && [ ! -L "$gofin_source/$gofin_name" ] || { echo 'Bundle has a missing or symbolic-link component.' >&2; exit 1; }
done
/usr/bin/awk 'NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ {bad=1}
 $2 != "gofin" && $2 != "gofin-broker" && $2 != "gofin-touch-id" && $2 != "install.sh" && $2 != "README.md" && $2 != "BUILDINFO" {bad=1}
 {if (++seen[$2] > 1) bad=1; count++} END {if (count != 6) bad=1; exit bad}' "$gofin_source/SHA256SUMS" || { echo 'Bundle checksum manifest is invalid.' >&2; exit 1; }
if [ "$gofin_verbose" = 1 ]; then
  echo "GoFin setup: bundle $gofin_source" >&2
  echo "GoFin setup: runtime $gofin_root/runtime" >&2
  echo "GoFin setup: PATH registration $gofin_path_file" >&2
fi
gofin_stage=$(/usr/bin/mktemp -d /Library/.gofin-rust-install.XXXXXX)
gofin_installed=0
gofin_configured=0
cleanup() {
  restore_companion_terminal || true
  /bin/rm -rf "$gofin_stage"
  # Initialization can commit configuration and then report a sync/store failure.
  # Preserve that uncertain durable result; never erase it as cancellation.
  if [ "$gofin_installed" = 1 ] && [ "$gofin_configured" = 0 ]; then
    if [ -e "$gofin_root/private/config.json" ] || [ -L "$gofin_root/private/config.json" ]; then
      echo 'Configuration may already be committed. Protected installation was preserved; inspect it before retrying setup.' >&2
    else
      /bin/rm -rf "$gofin_root"
    fi
  fi
}
trap cleanup EXIT
trap 'cancel_active_stage 129' HUP
trap 'cancel_active_stage 130' INT
trap 'cancel_active_stage 143' TERM
prepare_runtime() {
  /bin/mkdir "$gofin_stage/runtime" "$gofin_stage/private" "$gofin_stage/run"
  /bin/mkdir "$gofin_stage/runtime/authorization"
  /bin/mkdir "$gofin_stage/private/credentials"
  # Enrollment requires this fixed private lock before any configuration exists.
  : > "$gofin_stage/private/maintenance.lock"
  for gofin_name in gofin gofin-broker gofin-touch-id install.sh README.md BUILDINFO SHA256SUMS; do /bin/cp "$gofin_source/$gofin_name" "$gofin_stage/runtime/$gofin_name"; done
  /bin/cp "$gofin_stage/runtime/gofin-touch-id" "$gofin_stage/runtime/authorization/GoFin"
  /usr/bin/cmp -s "$gofin_stage/runtime/gofin-touch-id" "$gofin_stage/runtime/authorization/GoFin" || { echo 'Human-named authorization helper copy does not match the signed bundle helper.' >&2; return 1; }
  (cd "$gofin_stage/runtime" && /usr/bin/shasum -a 256 -c SHA256SUMS >/dev/null) || { echo 'Copied checksum verification failed.' >&2; return 1; }
  /usr/sbin/chown -R root:wheel "$gofin_stage"
  /bin/chmod -RN "$gofin_stage"
  /bin/chmod 755 "$gofin_stage" "$gofin_stage/runtime" "$gofin_stage/runtime/authorization" "$gofin_stage/run"
  /bin/chmod 700 "$gofin_stage/private" "$gofin_stage/private/credentials"
  /bin/chmod 600 "$gofin_stage/private/maintenance.lock"
  /bin/chmod 644 "$gofin_stage/runtime/README.md" "$gofin_stage/runtime/BUILDINFO" "$gofin_stage/runtime/SHA256SUMS"
  /bin/chmod 755 "$gofin_stage/runtime/gofin" "$gofin_stage/runtime/gofin-broker" "$gofin_stage/runtime/gofin-touch-id" "$gofin_stage/runtime/authorization/GoFin" "$gofin_stage/runtime/install.sh"
}
verify_binaries() {
  for gofin_binary in "$gofin_stage/runtime/gofin" "$gofin_stage/runtime/gofin-broker" "$gofin_stage/runtime/gofin-touch-id" "$gofin_stage/runtime/authorization/GoFin"; do
    /usr/bin/codesign --verify --strict "$gofin_binary"
    /usr/bin/codesign -d --verbose=4 "$gofin_binary" 2>&1 | /usr/bin/awk '/^CodeDirectory / && /flags=0x[0-9a-f]+\([^)]*runtime[,)]/ {valid=1} END {exit !valid}' || { echo 'Bundle lacks hardened signing.' >&2; return 1; }
    /usr/bin/otool -L "$gofin_binary" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}' || { echo 'Bundle uses unprotected non-OS libraries.' >&2; return 1; }
  done
}
run_stage 'Preparing protected runtime files' prepare_runtime
run_stage 'Verifying signed binaries and system libraries' verify_binaries
/bin/mv "$gofin_stage" "$gofin_root"
gofin_installed=1
if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[32m✓ Installing protected runtime\033[0m\n' >&2; else printf '✓ Installing protected runtime\n' >&2; fi
# Enrollment verifies all three descriptors. Keep stdout attached to the human
# terminal as well; suppressing it would make the protected input check fail.
if [ "$gofin_verbose" = 1 ]; then
  /Library/Fin/runtime/gofin-broker enrollment initialize --request "{\"approver_uid\":$gofin_uid}" --verbose
else
  /Library/Fin/runtime/gofin-broker enrollment initialize --request "{\"approver_uid\":$gofin_uid}"
fi
# Preserve approved configuration and journal even if later service setup fails.
gofin_configured=1
install_service() {
/bin/cat > "$gofin_plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>io.fin-cli.broker</string>
<key>ProgramArguments</key><array><string>/Library/Fin/runtime/gofin-broker</string></array>
<key>WorkingDirectory</key><string>/Library/Fin/runtime</string>
<key>UserName</key><string>root</string><key>GroupName</key><string>wheel</string>
<key>Umask</key><integer>63</integer>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>/usr/bin:/bin</string><key>LC_ALL</key><string>C</string></dict>
<key>Sockets</key><dict><key>Broker</key><dict>
<key>SockPathName</key><string>/Library/Fin/run/broker.sock</string>
<key>SockPathOwner</key><integer>$gofin_uid</integer><key>SockPathGroup</key><integer>$gofin_gid</integer><key>SockPathMode</key><integer>384</integer>
<key>SockType</key><string>stream</string>
</dict></dict>
<key>StandardOutPath</key><string>/Library/Fin/private/broker.stdout.log</string>
<key>StandardErrorPath</key><string>/Library/Fin/private/broker.stderr.log</string>
</dict></plist>
PLIST
/usr/sbin/chown root:wheel "$gofin_plist"
/bin/chmod -N "$gofin_plist"
/bin/chmod 644 "$gofin_plist"
/usr/bin/plutil -lint "$gofin_plist" >/dev/null
/bin/launchctl bootstrap system "$gofin_plist"
}
run_stage 'Registering protected local broker service' install_service
start_broker() {
  # Use the configured human's authenticated client, not a root socket client.
  # daemon start checks the broker reply and observed launchd process state.
  /usr/bin/sudo -n -H -u "#$gofin_uid" /usr/bin/env TERM=dumb \
    /Library/Fin/runtime/gofin daemon start
}
run_stage 'Starting and checking GoFin daemon' start_broker
# Register only after approved configuration and service bootstrap. Failures
# retain the protected configuration/journal for inspection rather than retry.
register_gofin_command() {
check_path_directories
[ ! -e "$gofin_path_file" ] && [ ! -L "$gofin_path_file" ] || { echo 'The GoFin PATH registration already exists. Protected installation was preserved; inspect it before retrying setup.' >&2; exit 1; }
(set -o noclobber; printf '%s\n' /Library/Fin/runtime > "$gofin_path_file") || { echo 'GoFin PATH registration failed. Protected installation was preserved; inspect it before retrying setup.' >&2; exit 1; }
/usr/sbin/chown root:wheel "$gofin_path_file"
/bin/chmod -N "$gofin_path_file"
/bin/chmod 644 "$gofin_path_file"
}
run_stage 'Registering GoFin for new Terminal sessions' register_gofin_command
# The completion helper runs as the configured interactive account so its
# getpwuid-based home and shell selection cannot be redirected by root env.
gofin_completion_marker=/Library/Fin/private/completion-owner
if (umask 077; printf '%s\n' "$gofin_uid" > "$gofin_completion_marker") &&
   /usr/sbin/chown root:wheel "$gofin_completion_marker" &&
   /bin/chmod -N "$gofin_completion_marker" &&
   /bin/chmod 600 "$gofin_completion_marker"; then
  gofin_completion_env=("TERM=${TERM:-dumb}")
  if [ "${NO_COLOR+x}" = x ]; then gofin_completion_env+=("NO_COLOR=$NO_COLOR"); fi
  if ! /usr/bin/sudo -n -H -u "#$gofin_uid" /usr/bin/env "${gofin_completion_env[@]}" \
    /Library/Fin/runtime/gofin internal-completions install; then
    echo 'GoFin is installed, but shell completion setup did not finish. Run `gofin internal-completions install` from your account.' >&2
  fi
else
  /bin/rm -f "$gofin_completion_marker" || true
  echo 'GoFin is installed, but shell completion setup was skipped because its owner marker could not be secured.' >&2
fi
echo 'GoFin installed and running. Next: open a new Terminal and run gofin connections add.'
offer_companion_skill
if [ "$gofin_verbose" = 1 ]; then
  echo 'GoFin setup: installed runtime: /Library/Fin/runtime' >&2
  echo 'GoFin setup: protected configuration and operation journal: /Library/Fin/private' >&2
  echo 'GoFin setup: broker socket: /Library/Fin/run/broker.sock' >&2
  echo 'GoFin setup: run sudo -K and close elevated shells after setup.' >&2
  echo 'GoFin setup: remove agent-readable credential copies after protected enrollment; rotate any keys that were disclosed.' >&2
  echo 'GoFin setup: run gofin doctor --json and gofin connections add.' >&2
fi
