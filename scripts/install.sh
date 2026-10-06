#!/bin/sh
# Download public release artifacts only. Protected setup stays in the compiled CLI.
# Keep execution inside fin_main: curl | sh must parse the function before stdin
# is reattached to the human terminal, and must exit without reading terminal text.
set -eu
umask 077
fin_json=0
fin_verbose=0
fin_work=''
fin_active_pid=''
fin_setup_attempted=0

fin_row() {
  if [ "${NO_COLOR+x}" != x ] && [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then
    printf '\033[%sm%s\033[0m\n' "$1" "$2" >&2
  else
    printf '%s\n' "$2" >&2
  fi
}
fin_die() {
  if [ "$fin_json" = 1 ]; then
    # Codes and messages are fixed strings; never interpolate argument contents.
    printf '{"error":{"code":"%s","message":"%s"}}\n' "$1" "$2"
  else
    fin_row 31 "✗ $2"
  fi
  exit 1
}
fin_cleanup() {
  if [ -n "$fin_active_pid" ]; then
    kill -TERM "$fin_active_pid" 2>/dev/null || true
    wait "$fin_active_pid" 2>/dev/null || true
  fi
  if [ "$fin_setup_attempted" = 1 ]; then /usr/bin/sudo -K || true; fi
  if [ -n "$fin_work" ]; then /bin/rm -rf "$fin_work"; fi
}
fin_stage() {
  fin_label=$1
  shift
  "$@" > "$fin_work/stage.log" 2>&1 &
  fin_active_pid=$!
  fin_frame='⠋'
  fin_animate=0
  if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then fin_animate=1; fi
  while kill -0 "$fin_active_pid" 2>/dev/null; do
    if [ "$fin_animate" = 1 ]; then
      printf '\r\033[2K\033[36m%s %s\033[0m' "$fin_frame" "$fin_label" >&2
      case "$fin_frame" in '⠋') fin_frame='⠙' ;; '⠙') fin_frame='⠹' ;; '⠹') fin_frame='⠸' ;; *) fin_frame='⠋' ;; esac
    fi
    sleep 0.12
  done
  if wait "$fin_active_pid"; then fin_status=0; else fin_status=$?; fi
  fin_active_pid=''
  if [ "$fin_animate" = 1 ]; then printf '\r\033[2K' >&2; fi
  if [ "$fin_verbose" = 1 ]; then cat "$fin_work/stage.log" >&2; fi
  [ "$fin_status" = 0 ] || fin_die RELEASE_VERIFICATION_FAILED "Release download or verification failed. Check the version, or retry with --verbose."
  fin_row 32 "✓ $fin_label"
}
fin_valid_version() {
  case "$1" in ''|*[!0-9A-Za-z.-]*) return 1 ;; esac
  printf '%s\n' "$1" | LC_ALL=C grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$' || return 1
  case "$1" in
    *-*) printf '%s\n' "${1#*-}" | LC_ALL=C awk -F. '{for(i=1;i<=NF;i++) if ($i ~ /^[0-9]+$/ && length($i)>1 && substr($i,1,1)=="0") exit 1}' ;;
  esac
}
fin_fetch() {
  # Track curl itself, rather than a background shell function whose child could
  # outlive cancellation. No human prompts are captured by this public download.
  fin_stage "$1" curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --tlsv1.2 --connect-timeout 15 --max-time 180 --output "$3" "$2"
}
fin_verify_archive() {
  awk -v asset="$fin_asset" '
    $2 == asset {
      if (NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/) exit 1
      print $1 "  " asset; count++
    }
    END {if (count != 1) exit 1}
  ' "$fin_work/SHA256SUMS" > "$fin_work/selected.sha256"
  (cd "$fin_work" && shasum -a 256 -c selected.sha256)
  tar -tzf "$fin_work/$fin_asset" > "$fin_work/entries"
  awk -v linux="$fin_linux" '
    BEGIN {
      allowed["bundle/"]=1; allowed["bundle/fin"]=1; allowed["bundle/README.md"]=1
      allowed["bundle/BUILDINFO"]=1; allowed["bundle/SHA256SUMS"]=1
      if (!linux) {allowed["bundle/fin-broker"]=1; allowed["bundle/fin-touch-id"]=1; allowed["bundle/install.sh"]=1}
    }
    !allowed[$0] || ++seen[$0] != 1 {bad=1}
    END {for (name in allowed) if (seen[name] != 1) bad=1; exit bad}
  ' "$fin_work/entries"
  # Validate types before extracting; a valid filename is not permission for a link.
  tar -tvzf "$fin_work/$fin_asset" > "$fin_work/types"
  awk '
    substr($0,1,1) != "-" && substr($0,1,1) != "d" {bad=1}
    END {exit bad}
  ' "$fin_work/types"
  tar -xzf "$fin_work/$fin_asset" --no-same-owner -C "$fin_work"
}
fin_verify_bundle() {
  awk -v linux="$fin_linux" '
    BEGIN {
      allowed["fin"]=1; allowed["README.md"]=1; allowed["BUILDINFO"]=1
      if (!linux) {allowed["fin-broker"]=1; allowed["fin-touch-id"]=1; allowed["install.sh"]=1}
    }
    NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ || !allowed[$2] || ++seen[$2] != 1 {bad=1}
    END {for (name in allowed) if (seen[name] != 1) bad=1; exit bad}
  ' "$fin_work/bundle/SHA256SUMS"
  (cd "$fin_work/bundle" && shasum -a 256 -c SHA256SUMS)
  awk -F= -v version="$fin_version" -v target="$fin_target" '
    $1 == "release_version" {if (++v != 1 || $2 != version) bad=1}
    $1 == "target" {if (++t != 1 || $2 != target) bad=1}
    $1 == "profile" {if (++p != 1 || $2 != "release") bad=1}
    $1 == "source_state" {if (++s != 1 || $2 != "clean") bad=1}
    $1 == "source_commit" {if (++c != 1 || length($2) != 40 || $2 ~ /[^0-9a-f]/) bad=1}
    END {if (v != 1 || t != 1 || p != 1 || s != 1 || c != 1) bad=1; exit bad}
  ' "$fin_work/bundle/BUILDINFO"
  [ -x "$fin_work/bundle/fin" ]
}
fin_main() {
  fin_version=''
  fin_destination=''
  fin_help=0
  # Select machine behavior even if an earlier argument is malformed.
  for fin_arg in "$@"; do if [ "$fin_arg" = --json ]; then fin_json=1; fi; done
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version) [ "$#" -ge 2 ] && [ -z "$fin_version" ] || fin_die INVALID_ARGUMENT "Provide one release version."; fin_version=$2; fin_valid_version "$fin_version" || fin_die INVALID_VERSION "Use a release tag such as v0.2.0-beta.1."; shift 2 ;;
      --download-only) [ "$#" -ge 2 ] && [ -z "$fin_destination" ] || fin_die INVALID_ARGUMENT "Provide one new download directory."; [ -n "$2" ] || fin_die INVALID_ARGUMENT "Provide a new download directory."; fin_destination=$2; shift 2 ;;
      --verbose) fin_verbose=1; shift ;;
      --json) shift ;;
      --help|-h) fin_help=1; shift ;;
      *) fin_die INVALID_ARGUMENT "Unknown option. Use --help." ;;
    esac
  done
  if [ "$fin_help" = 1 ]; then
    if [ "$fin_json" = 1 ]; then
      printf '%s\n' '{"command":"Fin release installer","options":["--version TAG","--download-only DIR","--verbose","--json"],"requires_human_terminal":true}'
    else
      printf '%s\n' 'Install Fin from a published GitHub release.' 'Usage: sh install.sh [--version vX.Y.Z] [--download-only NEW_DIR] [--verbose]' 'Default: latest stable release. Pin a version for a beta release.' 'macOS setup requires your own Terminal. Linux supports download-only discovery.'
    fi
    exit 0
  fi
  if [ -n "$fin_version" ]; then fin_valid_version "$fin_version" || fin_die INVALID_VERSION "Use a release tag such as v0.2.0-beta.1."; fi
  if [ "$fin_json" = 1 ]; then fin_die HUMAN_TERMINAL_REQUIRED "Run the installer yourself in macOS Terminal. No files were downloaded."; fi
  [ "$(id -u)" != 0 ] || fin_die HUMAN_ACCOUNT_REQUIRED "Run the installer as your own user, without sudo."
  fin_linux=0
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64|Darwin/aarch64) fin_target=aarch64-apple-darwin ;;
    Darwin/x86_64) fin_target=x86_64-apple-darwin ;;
    Linux/x86_64) fin_target=x86_64-unknown-linux-gnu; fin_linux=1 ;;
    *) fin_die UNSUPPORTED_PLATFORM "This platform has no Fin release bundle." ;;
  esac
  if [ -z "$fin_destination" ]; then
    [ "$fin_linux" = 0 ] || fin_die PROTECTED_PROVIDER_UNAVAILABLE "Linux supports discovery only. Use --download-only with a new directory."
    [ -t 1 ] && [ -t 2 ] || fin_die HUMAN_TERMINAL_REQUIRED "Run the installer yourself in macOS Terminal, without output redirection."
    # Only bootstrap input is reopened. The protected CLI still checks ancestry,
    # ownership and all terminal descriptors; this grants no approval or credentials.
    ( : < /dev/tty ) >/dev/null 2>&1 || fin_die HUMAN_TERMINAL_REQUIRED "A human terminal is required for setup."
    exec < /dev/tty
    [ -t 0 ] || fin_die HUMAN_TERMINAL_REQUIRED "A human terminal is required for setup."
    for fin_existing in /Library/Fin /Library/LaunchDaemons/io.fin-cli.broker.plist /private/etc/paths.d/fin; do
      [ ! -e "$fin_existing" ] && [ ! -L "$fin_existing" ] || fin_die ALREADY_INSTALLED "Fin is already installed. Setup preserves its credentials and financial state."
    done
  else
    case "$fin_destination" in /*) ;; *) fin_destination="$PWD/$fin_destination" ;; esac
    [ ! -e "$fin_destination" ] && [ ! -L "$fin_destination" ] || fin_die DESTINATION_EXISTS "Choose a new download directory; existing files are preserved."
  fi
  for fin_tool in curl tar shasum awk grep mktemp; do command -v "$fin_tool" >/dev/null || fin_die TOOL_UNAVAILABLE "A required download tool is missing: curl, tar, shasum, awk, grep or mktemp."; done
  fin_work=$(mktemp -d /tmp/fin-download.XXXXXX)
  trap fin_cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
  fin_repository=https://github.com/LimeChain/fin-cli
  if [ -z "$fin_version" ]; then
    fin_stage 'Finding latest stable release' curl --fail --silent --show-error --location --head --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout 15 --max-time 30 --output /dev/null --write-out '%{url_effective}' "$fin_repository/releases/latest"
    fin_latest=$(cat "$fin_work/stage.log")
    case "$fin_latest" in "$fin_repository/releases/tag/"*) fin_version=${fin_latest##*/} ;; *) fin_die RELEASE_UNAVAILABLE "GitHub did not resolve a stable release." ;; esac
    fin_valid_version "$fin_version" || fin_die INVALID_VERSION "The release tag is unsupported. Pin a valid version."
  fi
  fin_asset="fin-$fin_version-$fin_target"
  if [ "$fin_linux" = 1 ]; then fin_asset="$fin_asset-discovery"; fi
  fin_asset="$fin_asset.tar.gz"
  fin_release_url="$fin_repository/releases/download/$fin_version"
  fin_fetch 'Downloading release manifest' "$fin_release_url/SHA256SUMS" "$fin_work/SHA256SUMS"
  fin_fetch "Downloading Fin $fin_version" "$fin_release_url/$fin_asset" "$fin_work/$fin_asset"
  fin_stage 'Verifying release archive' fin_verify_archive
  fin_stage 'Verifying bundle contents' fin_verify_bundle
  if [ -n "$fin_destination" ]; then
    mkdir -m 700 "$fin_destination" || fin_die DESTINATION_EXISTS "The download directory could not be created. Existing files are preserved."
    cp -R "$fin_work/bundle/." "$fin_destination/"
    fin_row 32 '✓ Verified bundle saved. Inspect BUILDINFO and README.md before use.'
    exit 0
  fi
  # Never capture or pipe protected enrollment streams.
  fin_setup_attempted=1
  if [ "$fin_verbose" = 1 ]; then "$fin_work/bundle/fin" setup --verbose
  else "$fin_work/bundle/fin" setup
  fi
  exit 0
}
fin_main "$@"
