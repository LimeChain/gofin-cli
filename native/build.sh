#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ "$(uname -s)" != Darwin ]; then
  echo 'Fin Touch ID requires macOS.' >&2
  exit 1
fi
mkdir -p .build
/usr/bin/xcrun swiftc -O -framework AppKit -framework LocalAuthentication -framework CryptoKit FinTouchID.swift -o .build/fin-touch-id
# Ad-hoc hardened signing supports a local beta; developer release signing is separate.
# No debugging, DYLD injection, or library-validation exceptions are granted.
/usr/bin/codesign --force --sign - --options runtime .build/fin-touch-id
/usr/bin/codesign --verify --strict .build/fin-touch-id
echo 'Built native/.build/fin-touch-id (hardened runtime).'
