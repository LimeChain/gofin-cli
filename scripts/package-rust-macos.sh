#!/bin/sh
# Build an inspectable beta bundle; do not install or grant privileges.
set -eu
umask 077
gofin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
gofin_output=''
gofin_profile=release
gofin_verbose=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dev) [ "$gofin_profile" = release ] || { echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2; }; gofin_profile=debug; shift ;;
    --verbose) [ "$gofin_verbose" = 0 ] || { echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2; }; gofin_verbose=1; shift ;;
    -*) echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2 ;;
    *) [ -z "$gofin_output" ] || { echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2; }; gofin_output=$1; shift ;;
  esac
done
[ -n "$gofin_output" ] || gofin_output="$gofin_source/work/release-macos"
case "$gofin_output" in /*) ;; *) echo 'Use an absolute output path.' >&2; exit 2 ;; esac
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'macOS is required for this bundle.' >&2; exit 1; }
[ ! -e "$gofin_output" ] && [ ! -L "$gofin_output" ] || { echo 'Choose a new output directory.' >&2; exit 1; }
cd "$gofin_source"

gofin_success() {
  if [ "${NO_COLOR+x}" != x ] && [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then
    printf '\033[32m✓ %s\033[0m\n' "$1" >&2
  else
    printf '✓ %s\n' "$1" >&2
  fi
}

gofin_run_stage() {
  gofin_label=$1
  shift
  gofin_log=$(/usr/bin/mktemp /tmp/gofin-package.XXXXXX)
  if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then gofin_animate=1; else gofin_animate=0; fi
  "$@" >"$gofin_log" 2>&1 &
  gofin_pid=$!
  gofin_active_pid=$gofin_pid
  gofin_active_log=$gofin_log
  gofin_frame='⠋'
  while kill -0 "$gofin_pid" 2>/dev/null; do
    if [ "$gofin_animate" = 1 ]; then
      printf '\r\033[2K\033[36m%s %s\033[0m' "$gofin_frame" "$gofin_label" >&2
      case "$gofin_frame" in '⠋') gofin_frame='⠙' ;; '⠙') gofin_frame='⠹' ;; '⠹') gofin_frame='⠸' ;; *) gofin_frame='⠋' ;; esac
    fi
    sleep 0.12
  done
  if wait "$gofin_pid"; then
    if [ "$gofin_animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    gofin_success "$gofin_label"
    if [ "$gofin_verbose" = 1 ]; then /bin/cat "$gofin_log" >&2; fi
    gofin_active_pid=''
    gofin_active_log=''
    /bin/rm -f "$gofin_log"
  else
    gofin_status=$?
    if [ "$gofin_animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    if [ "${NO_COLOR+x}" != x ] && [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[31m✗ %s\033[0m\n' "$gofin_label" >&2; else printf '✗ %s\n' "$gofin_label" >&2; fi
    /bin/cat "$gofin_log" >&2
    gofin_active_pid=''
    gofin_active_log=''
    /bin/rm -f "$gofin_log"
    return "$gofin_status"
  fi
}
gofin_active_pid=''
gofin_active_log=''
gofin_cancel_stage() {
  gofin_status=$1
  if [ -n "$gofin_active_pid" ]; then
    kill -TERM "$gofin_active_pid" 2>/dev/null || true
    wait "$gofin_active_pid" 2>/dev/null || true
  fi
  if [ -n "$gofin_active_log" ]; then /bin/rm -f "$gofin_active_log"; fi
  exit "$gofin_status"
}
trap 'gofin_cancel_stage 129' HUP
trap 'gofin_cancel_stage 130' INT
trap 'gofin_cancel_stage 143' TERM

gofin_build() {
  if [ "$gofin_profile" = debug ]; then
    cargo build --locked -p gofin-cli -p gofin-broker
  else
    cargo build --locked --release -p gofin-cli -p gofin-broker
  fi
}
gofin_native_build() { /bin/sh native/build.sh; }
gofin_prepare_bundle() {
  /bin/mkdir -p "$gofin_output"
  /bin/cp "target/$gofin_profile/gofin" "target/$gofin_profile/gofin-broker" native/.build/gofin-touch-id "$gofin_output/"
  /bin/cp packaging/install.sh packaging/README.md "$gofin_output/"
  gofin_revision=$(git rev-parse HEAD)
  gofin_source_state=clean
  if ! git diff --quiet HEAD || [ -n "$(git ls-files --others --exclude-standard)" ]; then gofin_source_state=modified; fi
  printf 'source_commit=%s\nsource_state=%s\narchitecture=%s\nprofile=%s\n' \
    "$gofin_revision" "$gofin_source_state" "$(/usr/bin/uname -m)" "$gofin_profile" > "$gofin_output/BUILDINFO"
}
gofin_sign_bundle() {
  for gofin_binary in gofin gofin-broker gofin-touch-id; do
    /usr/bin/codesign --force --sign - --options runtime "$gofin_output/$gofin_binary"
    /usr/bin/codesign --verify --strict "$gofin_output/$gofin_binary"
    /usr/bin/otool -L "$gofin_output/$gofin_binary" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}'
  done
  (cd "$gofin_output" && /usr/bin/shasum -a 256 gofin gofin-broker gofin-touch-id install.sh README.md BUILDINFO > SHA256SUMS)
  /bin/chmod 755 "$gofin_output/gofin" "$gofin_output/gofin-broker" "$gofin_output/gofin-touch-id" "$gofin_output/install.sh"
  /bin/chmod 644 "$gofin_output/README.md" "$gofin_output/BUILDINFO" "$gofin_output/SHA256SUMS"
}

if [ "$gofin_profile" = release ]; then
  gofin_run_stage 'Checking offline CLI tests' cargo test --locked -p gofin-cli --test offline_cli
fi
gofin_run_stage "Compiling $gofin_profile CLI and broker" gofin_build
# Reuse the helper only for the fast local loop when its sources are unchanged.
if [ "$gofin_profile" = release ] || [ ! -f native/.build/gofin-touch-id ] ||
   [ native/GoFinTouchID.swift -nt native/.build/gofin-touch-id ] ||
   [ native/ApprovalPresentation.swift -nt native/.build/gofin-touch-id ] ||
   [ native/Info.plist -nt native/.build/gofin-touch-id ] ||
   [ native/build.sh -nt native/.build/gofin-touch-id ]; then
  gofin_run_stage 'Building the native approval helper' gofin_native_build
else
  gofin_success 'Reusing unchanged native approval helper'
fi
gofin_run_stage 'Preparing bundle files' gofin_prepare_bundle
gofin_run_stage 'Signing and verifying bundle binaries' gofin_sign_bundle
gofin_success "Bundle ready: $gofin_output"
