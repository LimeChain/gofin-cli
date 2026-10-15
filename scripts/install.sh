#!/bin/sh
# Download public release artifacts only. Protected setup stays in the compiled CLI.
# Keep execution inside gofin_main: curl | sh must parse the function before stdin
# is reattached to the human terminal, and must exit without reading terminal text.
set -eu
umask 077
gofin_json=0
gofin_verbose=0
gofin_work=''
gofin_active_pid=''
gofin_setup_attempted=0

gofin_row() {
  if [ "${NO_COLOR+x}" != x ] && [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then
    printf '\033[%sm%s\033[0m\n' "$1" "$2" >&2
  else
    printf '%s\n' "$2" >&2
  fi
}
gofin_die() {
  if [ "$gofin_json" = 1 ]; then
    # Codes and messages are fixed strings; never interpolate argument contents.
    printf '{"error":{"code":"%s","message":"%s"}}\n' "$1" "$2"
  else
    gofin_row 31 "✗ $2"
  fi
  exit 1
}
gofin_cleanup() {
  if [ -n "$gofin_active_pid" ]; then
    kill -TERM "$gofin_active_pid" 2>/dev/null || true
    wait "$gofin_active_pid" 2>/dev/null || true
  fi
  if [ "$gofin_setup_attempted" = 1 ]; then /usr/bin/sudo -K || true; fi
  if [ -n "$gofin_work" ]; then /bin/rm -rf "$gofin_work"; fi
}
gofin_stage() {
  gofin_label=$1
  shift
  "$@" > "$gofin_work/stage.log" 2>&1 &
  gofin_active_pid=$!
  gofin_frame='⠋'
  gofin_animate=0
  if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then gofin_animate=1; fi
  while kill -0 "$gofin_active_pid" 2>/dev/null; do
    if [ "$gofin_animate" = 1 ]; then
      printf '\r\033[2K\033[36m%s %s\033[0m' "$gofin_frame" "$gofin_label" >&2
      case "$gofin_frame" in '⠋') gofin_frame='⠙' ;; '⠙') gofin_frame='⠹' ;; '⠹') gofin_frame='⠸' ;; *) gofin_frame='⠋' ;; esac
    fi
    sleep 0.12
  done
  if wait "$gofin_active_pid"; then gofin_status=0; else gofin_status=$?; fi
  gofin_active_pid=''
  if [ "$gofin_animate" = 1 ]; then printf '\r\033[2K' >&2; fi
  if [ "$gofin_verbose" = 1 ]; then cat "$gofin_work/stage.log" >&2; fi
  [ "$gofin_status" = 0 ] || gofin_die RELEASE_VERIFICATION_FAILED "Release download or verification failed. Check the version, or retry with --verbose."
  gofin_row 32 "✓ $gofin_label"
}
gofin_valid_version() {
  case "$1" in ''|*[!0-9A-Za-z.-]*) return 1 ;; esac
  printf '%s\n' "$1" | LC_ALL=C grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$' || return 1
  case "$1" in
    *-*) printf '%s\n' "${1#*-}" | LC_ALL=C awk -F. '{for(i=1;i<=NF;i++) if ($i ~ /^[0-9]+$/ && length($i)>1 && substr($i,1,1)=="0") exit 1}' ;;
  esac
}
gofin_fetch() {
  # Track curl itself, rather than a background shell function whose child could
  # outlive cancellation. No human prompts are captured by this public download.
  gofin_stage "$1" curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --tlsv1.2 --connect-timeout 15 --max-time 180 --output "$3" "$2"
}
gofin_verify_archive() {
  awk -v asset="$gofin_asset" '
    $2 == asset {
      if (NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/) exit 1
      print $1 "  " asset; count++
    }
    END {if (count != 1) exit 1}
  ' "$gofin_work/SHA256SUMS" > "$gofin_work/selected.sha256"
  (cd "$gofin_work" && shasum -a 256 -c selected.sha256)
  tar -tzf "$gofin_work/$gofin_asset" > "$gofin_work/entries"
  awk -v linux="$gofin_linux" '
    BEGIN {
      allowed["bundle/"]=1; allowed["bundle/gofin"]=1; allowed["bundle/README.md"]=1
      allowed["bundle/BUILDINFO"]=1; allowed["bundle/SHA256SUMS"]=1
      if (!linux) {allowed["bundle/gofin-broker"]=1; allowed["bundle/gofin-touch-id"]=1; allowed["bundle/install.sh"]=1}
    }
    !allowed[$0] || ++seen[$0] != 1 {bad=1}
    END {for (name in allowed) if (seen[name] != 1) bad=1; exit bad}
  ' "$gofin_work/entries"
  # Validate types before extracting; a valid filename is not permission for a link.
  tar -tvzf "$gofin_work/$gofin_asset" > "$gofin_work/types"
  awk '
    substr($0,1,1) != "-" && substr($0,1,1) != "d" {bad=1}
    END {exit bad}
  ' "$gofin_work/types"
  tar -xzf "$gofin_work/$gofin_asset" --no-same-owner -C "$gofin_work"
}
gofin_verify_bundle() {
  awk -v linux="$gofin_linux" '
    BEGIN {
      allowed["gofin"]=1; allowed["README.md"]=1; allowed["BUILDINFO"]=1
      if (!linux) {allowed["gofin-broker"]=1; allowed["gofin-touch-id"]=1; allowed["install.sh"]=1}
    }
    NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ || !allowed[$2] || ++seen[$2] != 1 {bad=1}
    END {for (name in allowed) if (seen[name] != 1) bad=1; exit bad}
  ' "$gofin_work/bundle/SHA256SUMS"
  (cd "$gofin_work/bundle" && shasum -a 256 -c SHA256SUMS)
  awk -F= -v version="$gofin_version" -v target="$gofin_target" '
    $1 == "release_version" {if (++v != 1 || $2 != version) bad=1}
    $1 == "target" {if (++t != 1 || $2 != target) bad=1}
    $1 == "profile" {if (++p != 1 || $2 != "release") bad=1}
    $1 == "source_state" {if (++s != 1 || $2 != "clean") bad=1}
    $1 == "source_commit" {if (++c != 1 || length($2) != 40 || $2 ~ /[^0-9a-f]/) bad=1}
    END {if (v != 1 || t != 1 || p != 1 || s != 1 || c != 1) bad=1; exit bad}
  ' "$gofin_work/bundle/BUILDINFO"
  [ -x "$gofin_work/bundle/gofin" ]
}
gofin_main() {
  gofin_version=''
  gofin_destination=''
  gofin_help=0
  # Select machine behavior even if an earlier argument is malformed.
  for gofin_arg in "$@"; do if [ "$gofin_arg" = --json ]; then gofin_json=1; fi; done
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version) [ "$#" -ge 2 ] && [ -z "$gofin_version" ] || gofin_die INVALID_ARGUMENT "Provide one release version."; gofin_version=$2; gofin_valid_version "$gofin_version" || gofin_die INVALID_VERSION "Use a release tag such as v0.2.0-beta.1."; shift 2 ;;
      --download-only) [ "$#" -ge 2 ] && [ -z "$gofin_destination" ] || gofin_die INVALID_ARGUMENT "Provide one new download directory."; [ -n "$2" ] || gofin_die INVALID_ARGUMENT "Provide a new download directory."; gofin_destination=$2; shift 2 ;;
      --verbose) gofin_verbose=1; shift ;;
      --json) shift ;;
      --help|-h) gofin_help=1; shift ;;
      *) gofin_die INVALID_ARGUMENT "Unknown option. Use --help." ;;
    esac
  done
  if [ "$gofin_help" = 1 ]; then
    if [ "$gofin_json" = 1 ]; then
      printf '%s\n' '{"command":"GoFin release installer","options":["--version TAG","--download-only DIR","--verbose","--json"],"requires_human_terminal":true}'
    else
      printf '%s\n' 'Install GoFin from a published GitHub release.' 'Usage: sh install.sh [--version vX.Y.Z] [--download-only NEW_DIR] [--verbose]' 'Default: latest stable release. Pin a version for a beta release.' 'macOS setup requires your own Terminal. Linux supports download-only discovery.'
    fi
    exit 0
  fi
  if [ -n "$gofin_version" ]; then gofin_valid_version "$gofin_version" || gofin_die INVALID_VERSION "Use a release tag such as v0.2.0-beta.1."; fi
  if [ "$gofin_json" = 1 ]; then gofin_die HUMAN_TERMINAL_REQUIRED "Run the installer yourself in macOS Terminal. No files were downloaded."; fi
  [ "$(id -u)" != 0 ] || gofin_die HUMAN_ACCOUNT_REQUIRED "Run the installer as your own user, without sudo."
  gofin_linux=0
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64|Darwin/aarch64) gofin_target=aarch64-apple-darwin ;;
    Darwin/x86_64) gofin_target=x86_64-apple-darwin ;;
    Linux/x86_64) gofin_target=x86_64-unknown-linux-gnu; gofin_linux=1 ;;
    *) gofin_die UNSUPPORTED_PLATFORM "This platform has no GoFin release bundle." ;;
  esac
  if [ -z "$gofin_destination" ]; then
    [ "$gofin_linux" = 0 ] || gofin_die PROTECTED_PROVIDER_UNAVAILABLE "Linux supports discovery only. Use --download-only with a new directory."
    [ -t 1 ] && [ -t 2 ] || gofin_die HUMAN_TERMINAL_REQUIRED "Run the installer yourself in macOS Terminal, without output redirection."
    # Only bootstrap input is reopened. The protected CLI still checks ancestry,
    # ownership and all terminal descriptors; this grants no approval or credentials.
    ( : < /dev/tty ) >/dev/null 2>&1 || gofin_die HUMAN_TERMINAL_REQUIRED "A human terminal is required for setup."
    exec < /dev/tty
    [ -t 0 ] || gofin_die HUMAN_TERMINAL_REQUIRED "A human terminal is required for setup."
    for gofin_existing in /Library/Fin /Library/LaunchDaemons/io.fin-cli.broker.plist /private/etc/paths.d/fin; do
      [ ! -e "$gofin_existing" ] && [ ! -L "$gofin_existing" ] || gofin_die ALREADY_INSTALLED "GoFin is already installed. Setup preserves its credentials and financial state."
    done
  else
    case "$gofin_destination" in /*) ;; *) gofin_destination="$PWD/$gofin_destination" ;; esac
    [ ! -e "$gofin_destination" ] && [ ! -L "$gofin_destination" ] || gofin_die DESTINATION_EXISTS "Choose a new download directory; existing files are preserved."
  fi
  for gofin_tool in curl tar shasum awk grep mktemp; do command -v "$gofin_tool" >/dev/null || gofin_die TOOL_UNAVAILABLE "A required download tool is missing: curl, tar, shasum, awk, grep or mktemp."; done
  gofin_work=$(mktemp -d /tmp/gofin-download.XXXXXX)
  trap gofin_cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
  gofin_repository=https://github.com/LimeChain/fin-cli
  if [ -z "$gofin_version" ]; then
    gofin_stage 'Finding latest stable release' curl --fail --silent --show-error --location --head --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout 15 --max-time 30 --output /dev/null --write-out '%{url_effective}' "$gofin_repository/releases/latest"
    gofin_latest=$(cat "$gofin_work/stage.log")
    case "$gofin_latest" in "$gofin_repository/releases/tag/"*) gofin_version=${gofin_latest##*/} ;; *) gofin_die RELEASE_UNAVAILABLE "GitHub did not resolve a stable release." ;; esac
    gofin_valid_version "$gofin_version" || gofin_die INVALID_VERSION "The release tag is unsupported. Pin a valid version."
  fi
  gofin_asset="gofin-$gofin_version-$gofin_target"
  if [ "$gofin_linux" = 1 ]; then gofin_asset="$gofin_asset-discovery"; fi
  gofin_asset="$gofin_asset.tar.gz"
  gofin_release_url="$gofin_repository/releases/download/$gofin_version"
  gofin_fetch 'Downloading release manifest' "$gofin_release_url/SHA256SUMS" "$gofin_work/SHA256SUMS"
  gofin_fetch "Downloading GoFin $gofin_version" "$gofin_release_url/$gofin_asset" "$gofin_work/$gofin_asset"
  gofin_stage 'Verifying release archive' gofin_verify_archive
  gofin_stage 'Verifying bundle contents' gofin_verify_bundle
  if [ -n "$gofin_destination" ]; then
    mkdir -m 700 "$gofin_destination" || gofin_die DESTINATION_EXISTS "The download directory could not be created. Existing files are preserved."
    cp -R "$gofin_work/bundle/." "$gofin_destination/"
    gofin_row 32 '✓ Verified bundle saved. Inspect BUILDINFO and README.md before use.'
    exit 0
  fi
  # Never capture or pipe protected enrollment streams.
  gofin_setup_attempted=1
  if [ "$gofin_verbose" = 1 ]; then "$gofin_work/bundle/gofin" setup --verbose
  else "$gofin_work/bundle/gofin" setup
  fi
  exit 0
}
gofin_main "$@"
