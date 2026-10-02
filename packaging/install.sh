#!/bin/bash
# Human initial setup. Never overwrite existing financial state.
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH
fin_uid=''
fin_gid=''
usage() { echo 'Run fin setup from a reviewed macOS bundle in your own Terminal.' >&2; exit 2; }
while [ "$#" -gt 0 ]; do
  [ "$#" -ge 2 ] || usage
  case "$1" in --approver-uid) fin_uid=$2 ;; --approver-gid) fin_gid=$2 ;; *) usage ;; esac
  shift 2
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
for fin_existing in "$fin_root" "$fin_plist" /usr/local/bin/fin; do
  [ ! -e "$fin_existing" ] && [ ! -L "$fin_existing" ] || { echo 'An existing Fin installation needs a reviewed upgrade; its state was preserved.' >&2; exit 1; }
done
for fin_name in fin fin-broker fin-touch-id install.sh README.md BUILDINFO SHA256SUMS; do
  [ -f "$fin_source/$fin_name" ] && [ ! -L "$fin_source/$fin_name" ] || { echo 'Bundle has a missing or symbolic-link component.' >&2; exit 1; }
done
/usr/bin/awk 'NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ {bad=1}
 $2 != "fin" && $2 != "fin-broker" && $2 != "fin-touch-id" && $2 != "install.sh" && $2 != "README.md" && $2 != "BUILDINFO" {bad=1}
 {if (++seen[$2] > 1) bad=1; count++} END {if (count != 6) bad=1; exit bad}' "$fin_source/SHA256SUMS" || { echo 'Bundle checksum manifest is invalid.' >&2; exit 1; }
echo 'Fin installs compiled binaries in /Library/Fin/runtime, private state, a launchd socket service, and /usr/local/bin/fin.'
echo 'Touch ID approves initial configuration. Each later financial write needs a separate fresh approval.'
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
trap 'exit 1' HUP INT TERM
/bin/mkdir "$fin_stage/runtime" "$fin_stage/private" "$fin_stage/run"
/bin/mkdir "$fin_stage/private/credentials"
# Enrollment requires this fixed private lock before any configuration exists.
: > "$fin_stage/private/maintenance.lock"
for fin_name in fin fin-broker fin-touch-id install.sh README.md BUILDINFO SHA256SUMS; do /bin/cp "$fin_source/$fin_name" "$fin_stage/runtime/$fin_name"; done
(cd "$fin_stage/runtime" && /usr/bin/shasum -a 256 -c SHA256SUMS >/dev/null) || { echo 'Copied checksum verification failed.' >&2; exit 1; }
/usr/sbin/chown -R root:wheel "$fin_stage"
/bin/chmod -RN "$fin_stage"
/bin/chmod 755 "$fin_stage" "$fin_stage/runtime" "$fin_stage/run"
/bin/chmod 700 "$fin_stage/private" "$fin_stage/private/credentials"
/bin/chmod 600 "$fin_stage/private/maintenance.lock"
/bin/chmod 644 "$fin_stage/runtime/README.md" "$fin_stage/runtime/BUILDINFO" "$fin_stage/runtime/SHA256SUMS"
/bin/chmod 755 "$fin_stage/runtime/fin" "$fin_stage/runtime/fin-broker" "$fin_stage/runtime/fin-touch-id" "$fin_stage/runtime/install.sh"
for fin_name in fin fin-broker fin-touch-id; do
  /usr/bin/codesign --verify --strict "$fin_stage/runtime/$fin_name"
  /usr/bin/codesign -d --verbose=4 "$fin_stage/runtime/$fin_name" 2>&1 | /usr/bin/awk '/^CodeDirectory / && /flags=0x[0-9a-f]+\([^)]*runtime[,)]/ {valid=1} END {exit !valid}' || { echo 'Bundle lacks hardened signing.' >&2; exit 1; }
  /usr/bin/otool -L "$fin_stage/runtime/$fin_name" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}' || { echo 'Bundle uses unprotected non-OS libraries.' >&2; exit 1; }
done
/bin/mv "$fin_stage" "$fin_root"
fin_installed=1
/Library/Fin/runtime/fin-broker enrollment initialize --request "{\"approver_uid\":$fin_uid}"
# Preserve approved configuration and journal even if later service setup fails.
fin_configured=1
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
# The optional convenience alias must not turn Homebrew directory ownership
# into a partially configured installation. Never change an existing owner's
# directory or follow a symbolic link in this path.
fin_cli=/Library/Fin/runtime/fin
if [ ! -e /usr/local ] && [ ! -L /usr/local ]; then /bin/mkdir /usr/local; fi
if [ -d /usr/local ] && [ ! -L /usr/local ] && [ "$(/usr/bin/stat -f '%u' /usr/local)" = 0 ]; then
  if [ ! -e /usr/local/bin ] && [ ! -L /usr/local/bin ]; then /bin/mkdir /usr/local/bin; fi
  if [ -d /usr/local/bin ] && [ ! -L /usr/local/bin ] && [ "$(/usr/bin/stat -f '%u' /usr/local/bin)" = 0 ]; then
    # Reject group/world writable directories and extended ACLs for this alias.
    if /bin/ls -ld /usr/local /usr/local/bin | /usr/bin/awk '{if (substr($1,6,1)=="w" || substr($1,9,1)=="w" || index($1,"+")) bad=1} END {exit bad}'; then
      /bin/ln -s /Library/Fin/runtime/fin /usr/local/bin/fin
      fin_cli=fin
    fi
  fi
fi
/bin/launchctl bootstrap system "$fin_plist"
echo "Fin installed. Run $fin_cli daemon start, $fin_cli doctor --json, then $fin_cli connections add yourself."
echo 'Human: run sudo -K and close elevated shells. Verify the actual agent cannot obtain noninteractive sudo before asserting isolation.'
echo 'Remove agent-readable credential copies after protected enrollment. Old disclosures are not retroactively protected; rotate exposed keys where needed.'
