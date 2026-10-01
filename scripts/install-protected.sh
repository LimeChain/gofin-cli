#!/bin/sh
# Run this reviewed installer as a HUMAN administrator, outside the agent's tools.
# It never prints credential contents. Initial privileged installation is the trust bootstrap.
set -eu
umask 077
usage() {
  echo 'Usage: sudo /bin/sh scripts/install-protected.sh --node /absolute/node --config /absolute/config.json --credentials /absolute/credentials.env --approver-uid 501' >&2
  exit 2
}
if [ "$(/usr/bin/id -u)" != 0 ]; then
  echo 'Human administrator authorization is required to install the protected Fin broker.' >&2
  exit 1
fi
if [ "$(/usr/bin/uname -s)" != Darwin ]; then
  echo 'The protected beta requires macOS.' >&2
  exit 1
fi
fin_node=''
fin_config=''
fin_credentials=''
fin_approver_uid=''
while [ "$#" -gt 0 ]; do
  [ "$#" -ge 2 ] || usage
  case "$1" in
    --node) fin_node=$2 ;;
    --config) fin_config=$2 ;;
    --credentials) fin_credentials=$2 ;;
    --approver-uid) fin_approver_uid=$2 ;;
    *) usage ;;
  esac
  shift 2
done
for fin_path in "$fin_node" "$fin_config" "$fin_credentials"; do
  case "$fin_path" in /*) ;; *) usage ;; esac
  [ -f "$fin_path" ] || { echo 'A required input file is missing.' >&2; exit 1; }
done
case "$fin_approver_uid" in ''|*[!0-9]*) usage ;; esac
[ "$fin_approver_uid" -ge 501 ] || usage
/usr/bin/id -nu "$fin_approver_uid" >/dev/null
fin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
[ -f "$fin_source/dist/broker.js" ] || { echo 'Run npm run build before installation.' >&2; exit 1; }
[ -x "$fin_source/native/.build/fin-touch-id" ] || { echo 'Run sh native/build.sh before installation.' >&2; exit 1; }
if [ -e /Library/Fin ] || [ -L /Library/Fin ]; then
  echo '/Library/Fin already exists. Stop and review the installed broker before performing an administrator-managed upgrade.' >&2
  exit 1
fi
fin_plist=/Library/LaunchDaemons/io.fin-cli.broker.plist
if [ -e "$fin_plist" ] || [ -L "$fin_plist" ]; then
  echo 'LaunchDaemon identity already exists; have the administrator resolve it before installing Fin.' >&2
  exit 1
fi
# Homebrew-linked dylibs can be user-writable. Require a standalone Node build using OS libraries.
if /usr/bin/otool -L "$fin_node" | /usr/bin/awk 'NR > 1 { if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) invalid=1 } END {exit invalid}'; then :; else
  echo 'Node must link only /usr/lib or /System/Library dependencies; use a standalone Node distribution.' >&2
  exit 1
fi
fin_stage=$(/usr/bin/mktemp -d /Library/.fin-install.XXXXXX)
trap '/bin/rm -rf "$fin_stage"' EXIT HUP INT TERM
/bin/mkdir -p "$fin_stage/runtime/native" "$fin_stage/private"
/bin/cp "$fin_node" "$fin_stage/runtime/node"
/bin/cp -R "$fin_source/dist" "$fin_stage/runtime/dist"
/bin/cp "$fin_source/package.json" "$fin_stage/runtime/package.json"
/bin/cp "$fin_source/package-lock.json" "$fin_stage/runtime/package-lock.json"
/bin/cp -R "$fin_source/node_modules" "$fin_stage/runtime/node_modules"
# Development command shims are not needed by the broker and include mutable symlinks.
/bin/rm -rf "$fin_stage/runtime/node_modules/.bin"
/bin/cp "$fin_source/native/.build/fin-touch-id" "$fin_stage/runtime/native/fin-touch-id"
/bin/cp "$fin_config" "$fin_stage/private/config.json"
/bin/cp "$fin_credentials" "$fin_stage/private/credentials.env"
/usr/bin/printf '{"version":1,"approverUid":%s}\n' "$fin_approver_uid" > "$fin_stage/private/approval.json"
# Never preserve source ownership or extended ACL grants into the protected boundary.
/usr/sbin/chown -R root:wheel "$fin_stage"
/bin/chmod -RN "$fin_stage"
/bin/chmod -R go-w "$fin_stage/runtime"
/usr/bin/find "$fin_stage/runtime" -type d -exec /bin/chmod 755 {} +
/usr/bin/find "$fin_stage/runtime" -type f -exec /bin/chmod 644 {} +
/bin/chmod 755 "$fin_stage/runtime/node" "$fin_stage/runtime/native/fin-touch-id"
/bin/chmod 755 "$fin_stage"
/bin/chmod 711 "$fin_stage/private"
/bin/chmod 600 "$fin_stage/private/config.json" "$fin_stage/private/credentials.env" "$fin_stage/private/approval.json"
if [ -n "$(/usr/bin/find "$fin_stage/runtime" -type l -print -quit)" ]; then
  echo 'Runtime contains a symbolic link. Remove or resolve it in the reviewed installation artifact.' >&2
  exit 1
fi
/usr/bin/codesign --verify --strict "$fin_stage/runtime/native/fin-touch-id"
/bin/mv "$fin_stage" /Library/Fin
trap - EXIT HUP INT TERM
# This launch configuration is root-owned and cannot be replaced from the agent session.
/bin/cat > "$fin_plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>io.fin-cli.broker</string>
  <key>ProgramArguments</key><array><string>/Library/Fin/runtime/node</string><string>/Library/Fin/runtime/dist/broker.js</string></array>
  <key>WorkingDirectory</key><string>/Library/Fin/runtime</string>
  <key>UserName</key><string>root</string>
  <key>GroupName</key><string>wheel</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>/usr/bin:/bin</string><key>NODE_OPTIONS</key><string></string><key>NODE_PATH</key><string></string><key>DYLD_INSERT_LIBRARIES</key><string></string><key>DYLD_LIBRARY_PATH</key><string></string></dict>
  <key>Umask</key><integer>63</integer>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Library/Fin/private/broker.stdout.log</string>
  <key>StandardErrorPath</key><string>/Library/Fin/private/broker.stderr.log</string>
</dict></plist>
PLIST
/usr/sbin/chown root:wheel "$fin_plist"
/bin/chmod -N "$fin_plist"
/bin/chmod 644 "$fin_plist"
/bin/launchctl bootstrap system "$fin_plist"
echo 'Installed protected Fin broker. Verify doctor, credential denial, and human Touch ID before live writes.'
echo 'Human: after this installer exits, run sudo -K in the installation session and close elevated shells. Verify the actual agent session cannot use noninteractive sudo before asserting the normal-user boundary.'
echo 'After verifying the protected import and its permissions, the human must remove all agent-readable execution-credential copies before claiming current credential isolation.'
echo 'Earlier plaintext access is not retroactively protected. Recommend revocation or rotation for prior disclosure or suspected compromise.'
