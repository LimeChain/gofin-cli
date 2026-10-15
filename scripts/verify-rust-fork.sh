#!/bin/sh
# Isolated fork verification only. The Python runner owns each Anvil process.
set -eu
gofin_source=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
exec python3 "$gofin_source/scripts/verify-rust-fork.py" "$@"
