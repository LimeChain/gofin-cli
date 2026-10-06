#!/bin/sh
# Build an inspectable beta bundle; do not install or grant privileges.
set -eu
umask 077
fin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
fin_output=''
fin_profile=release
fin_verbose=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dev) [ "$fin_profile" = release ] || { echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2; }; fin_profile=debug; shift ;;
    --verbose) [ "$fin_verbose" = 0 ] || { echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2; }; fin_verbose=1; shift ;;
    -*) echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2 ;;
    *) [ -z "$fin_output" ] || { echo 'Usage: package-rust-macos.sh /new/output/path [--dev] [--verbose]' >&2; exit 2; }; fin_output=$1; shift ;;
  esac
done
[ -n "$fin_output" ] || fin_output="$fin_source/work/release-macos"
case "$fin_output" in /*) ;; *) echo 'Use an absolute output path.' >&2; exit 2 ;; esac
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'macOS is required for this bundle.' >&2; exit 1; }
[ ! -e "$fin_output" ] && [ ! -L "$fin_output" ] || { echo 'Choose a new output directory.' >&2; exit 1; }
cd "$fin_source"

fin_success() {
  if [ "${NO_COLOR+x}" != x ] && [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then
    printf '\033[32m✓ %s\033[0m\n' "$1" >&2
  else
    printf '✓ %s\n' "$1" >&2
  fi
}

fin_run_stage() {
  fin_label=$1
  shift
  fin_log=$(/usr/bin/mktemp /tmp/fin-package.XXXXXX)
  if [ -t 2 ] && [ "${NO_COLOR+x}" != x ] && [ "${TERM:-dumb}" != dumb ]; then fin_animate=1; else fin_animate=0; fi
  "$@" >"$fin_log" 2>&1 &
  fin_pid=$!
  fin_active_pid=$fin_pid
  fin_active_log=$fin_log
  fin_frame='⠋'
  while kill -0 "$fin_pid" 2>/dev/null; do
    if [ "$fin_animate" = 1 ]; then
      printf '\r\033[2K\033[36m%s %s\033[0m' "$fin_frame" "$fin_label" >&2
      case "$fin_frame" in '⠋') fin_frame='⠙' ;; '⠙') fin_frame='⠹' ;; '⠹') fin_frame='⠸' ;; *) fin_frame='⠋' ;; esac
    fi
    sleep 0.12
  done
  if wait "$fin_pid"; then
    if [ "$fin_animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    fin_success "$fin_label"
    if [ "$fin_verbose" = 1 ]; then /bin/cat "$fin_log" >&2; fi
    fin_active_pid=''
    fin_active_log=''
    /bin/rm -f "$fin_log"
  else
    fin_status=$?
    if [ "$fin_animate" = 1 ]; then printf '\r\033[2K' >&2; fi
    if [ "${NO_COLOR+x}" != x ] && [ -t 2 ] && [ "${TERM:-dumb}" != dumb ]; then printf '\033[31m✗ %s\033[0m\n' "$fin_label" >&2; else printf '✗ %s\n' "$fin_label" >&2; fi
    /bin/cat "$fin_log" >&2
    fin_active_pid=''
    fin_active_log=''
    /bin/rm -f "$fin_log"
    return "$fin_status"
  fi
}
fin_active_pid=''
fin_active_log=''
fin_cancel_stage() {
  fin_status=$1
  if [ -n "$fin_active_pid" ]; then
    kill -TERM "$fin_active_pid" 2>/dev/null || true
    wait "$fin_active_pid" 2>/dev/null || true
  fi
  if [ -n "$fin_active_log" ]; then /bin/rm -f "$fin_active_log"; fi
  exit "$fin_status"
}
trap 'fin_cancel_stage 129' HUP
trap 'fin_cancel_stage 130' INT
trap 'fin_cancel_stage 143' TERM

fin_build() {
  if [ "$fin_profile" = debug ]; then
    cargo build --locked -p fin-cli -p fin-broker
  else
    cargo build --locked --release -p fin-cli -p fin-broker
  fi
}
fin_native_build() { /bin/sh native/build.sh; }
fin_prepare_bundle() {
  /bin/mkdir -p "$fin_output"
  /bin/cp "target/$fin_profile/fin" "target/$fin_profile/fin-broker" native/.build/fin-touch-id "$fin_output/"
  /bin/cp packaging/install.sh packaging/README.md "$fin_output/"
  fin_revision=$(git rev-parse HEAD)
  fin_source_state=clean
  if ! git diff --quiet HEAD || [ -n "$(git ls-files --others --exclude-standard)" ]; then fin_source_state=modified; fi
  printf 'source_commit=%s\nsource_state=%s\narchitecture=%s\nprofile=%s\n' \
    "$fin_revision" "$fin_source_state" "$(/usr/bin/uname -m)" "$fin_profile" > "$fin_output/BUILDINFO"
}
fin_sign_bundle() {
  for fin_binary in fin fin-broker fin-touch-id; do
    /usr/bin/codesign --force --sign - --options runtime "$fin_output/$fin_binary"
    /usr/bin/codesign --verify --strict "$fin_output/$fin_binary"
    /usr/bin/otool -L "$fin_output/$fin_binary" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}'
  done
  (cd "$fin_output" && /usr/bin/shasum -a 256 fin fin-broker fin-touch-id install.sh README.md BUILDINFO > SHA256SUMS)
  /bin/chmod 755 "$fin_output/fin" "$fin_output/fin-broker" "$fin_output/fin-touch-id" "$fin_output/install.sh"
  /bin/chmod 644 "$fin_output/README.md" "$fin_output/BUILDINFO" "$fin_output/SHA256SUMS"
}

if [ "$fin_profile" = release ]; then
  fin_run_stage 'Checking offline CLI tests' cargo test --locked -p fin-cli --test offline_cli
fi
fin_run_stage "Compiling $fin_profile CLI and broker" fin_build
# Reuse the helper only for the fast local loop when its sources are unchanged.
if [ "$fin_profile" = release ] || [ ! -f native/.build/fin-touch-id ] ||
   [ native/FinTouchID.swift -nt native/.build/fin-touch-id ] ||
   [ native/ApprovalPresentation.swift -nt native/.build/fin-touch-id ] ||
   [ native/Info.plist -nt native/.build/fin-touch-id ] ||
   [ native/build.sh -nt native/.build/fin-touch-id ]; then
  fin_run_stage 'Building the native approval helper' fin_native_build
else
  fin_success 'Reusing unchanged native approval helper'
fi
fin_run_stage 'Preparing bundle files' fin_prepare_bundle
fin_run_stage 'Signing and verifying bundle binaries' fin_sign_bundle
fin_success "Bundle ready: $fin_output"
