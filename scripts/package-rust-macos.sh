#!/bin/sh
# Build an inspectable beta bundle; do not install or grant privileges.
set -eu
umask 077
fin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
fin_output=${1:-"$fin_source/work/release-macos"}
case "$fin_output" in /*) ;; *) echo 'Use an absolute output path.' >&2; exit 2 ;; esac
[ "$(/usr/bin/uname -s)" = Darwin ] || { echo 'macOS is required for this bundle.' >&2; exit 1; }
[ ! -e "$fin_output" ] && [ ! -L "$fin_output" ] || { echo 'Choose a new output directory.' >&2; exit 1; }
cd "$fin_source"
cargo test --locked -p fin-cli --test offline_cli
cargo build --locked --release -p fin-cli -p fin-broker
/bin/sh native/build.sh
/bin/mkdir -p "$fin_output"
/bin/cp target/release/fin target/release/fin-broker native/.build/fin-touch-id "$fin_output/"
/bin/cp packaging/install.sh packaging/README.md "$fin_output/"
fin_revision=$(git rev-parse HEAD)
fin_source_state=clean
if ! git diff --quiet HEAD || [ -n "$(git ls-files --others --exclude-standard)" ]; then
  fin_source_state=modified
fi
printf 'source_commit=%s\nsource_state=%s\narchitecture=%s\n' \
  "$fin_revision" "$fin_source_state" "$(/usr/bin/uname -m)" > "$fin_output/BUILDINFO"
for fin_binary in fin fin-broker fin-touch-id; do
  /usr/bin/codesign --force --sign - --options runtime "$fin_output/$fin_binary"
  /usr/bin/codesign --verify --strict "$fin_output/$fin_binary"
  /usr/bin/otool -L "$fin_output/$fin_binary" | /usr/bin/awk 'NR > 1 {if ($1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\//) bad=1} END {exit bad}'
done
cd "$fin_output"
/usr/bin/shasum -a 256 fin fin-broker fin-touch-id install.sh README.md BUILDINFO > SHA256SUMS
/bin/chmod 755 fin fin-broker fin-touch-id install.sh
/bin/chmod 644 README.md BUILDINFO SHA256SUMS
echo "Built beta bundle: $fin_output"
echo 'No installation, credential access, approval, or financial movement was performed.'
